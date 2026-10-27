use std::{
    collections::HashSet,
    fs::File,
    future::Future,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use tokio::{
    sync::{mpsc, Mutex, Semaphore},
    time::Instant,
};
use tracing::log;
use zip::write::FileOptions;

use crate::{
    config::{archive_path, partial_archive_path, CONFIG},
    services::{
        downloader::{download, is_rate_limited},
        utils::get_filename,
    },
    structures::{CreateTask, ObjectType, Task, TaskStatus},
    views::{get_task, put_task, DEDUP_INDEX},
};

use super::{
    library_client::{get_author_books, get_sequence_books, get_translator_books, Book, Page},
    utils::{generate_token, get_key},
};

/// A task is failed when it makes no progress for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Absolute upper bound for one task, regardless of progress.
const MAX_TASK_DURATION: Duration = Duration::from_secs(3 * 60 * 60);
/// How often the watchdog checks the limits above.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(10);
/// Number of books downloaded in parallel.
const DOWNLOAD_CONCURRENCY: usize = 4;
/// Deflate level for the archive (6 = zlib default, good speed/size balance).
const COMPRESSION_LEVEL: i64 = 6;

static SEMAPHORE: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(CONFIG.max_concurrent_tasks)));
/// Tasks that were accepted but have not yet obtained a concurrency slot.
static WAITING: AtomicUsize = AtomicUsize::new(0);
/// Serializes dedup lookup + task registration so identical concurrent
/// requests cannot create two tasks.
static CREATE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Debug)]
pub enum CreateTaskError {
    QueueFull,
}

/// Per-task context: token plus a "last progress" timestamp used by the
/// idle watchdog.
#[derive(Clone)]
pub struct TaskCtx {
    pub token: String,
    started: Instant,
    last_progress_ms: Arc<AtomicU64>,
}

impl TaskCtx {
    pub fn new(token: String) -> Self {
        Self {
            token,
            started: Instant::now(),
            last_progress_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn touch(&self) {
        self.last_progress_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_progress_ms.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }
}

/// Removes the listed files on drop unless disarmed. Covers failure,
/// timeout (future dropped) and panic paths.
struct FileGuard {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl FileGuard {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        if self.armed {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

pub async fn get_books<Fut>(
    object_id: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
    books_getter: fn(id: u32, page: u32, allowed_langs: SmallVec<[SmartString; 3]>) -> Fut,
    file_format: SmartString,
) -> Result<Vec<Book>, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: std::future::Future<Output = Result<Page<Book>, Box<dyn std::error::Error + Send + Sync>>>,
{
    let mut result: Vec<Book> = vec![];

    let first_page = books_getter(object_id, 1, allowed_langs.clone()).await?;

    result.extend(first_page.items);

    let page_count = first_page.pages;

    for current_page in 2..=page_count {
        let page = books_getter(object_id, current_page, allowed_langs.clone()).await?;
        result.extend(page.items);
    }

    let result = result
        .iter()
        .filter(|book| book.available_types.contains(&file_format.to_string()))
        .cloned()
        .collect();

    Ok(result)
}

pub async fn set_task_error(token: String, error_message: String) {
    let task = Task {
        id: token.clone(),
        status: TaskStatus::Failed,
        status_description: "Ошибка!".to_string(),
        error_message: Some(error_message),
        result_filename: None,
        content_size: None,
    };

    put_task(task).await;
}

/// Updates the human-readable status of a running task and records progress
/// for the idle watchdog.
pub async fn set_progress_description(ctx: &TaskCtx, description: String) {
    ctx.touch();

    let task = Task {
        id: ctx.token.clone(),
        status: TaskStatus::InProgress,
        status_description: description,
        error_message: None,
        result_filename: None,
        content_size: None,
    };

    put_task(task).await;
}

struct WriterItem {
    book_id: u64,
    file: File,
    filename: String,
}

struct WriteSummary {
    files: usize,
}

pub struct ArchiveSummary {
    pub written: usize,
    pub skipped: usize,
    pub total: usize,
}

/// Returns a zip entry name not yet in `used`. On collision the book id is
/// appended to the stem (`name_{id}.ext`) instead of dropping the book.
fn unique_name(used: &mut HashSet<String>, name: &str, book_id: u64) -> String {
    if used.insert(name.to_string()) {
        return name.to_string();
    }

    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };

    let mut candidate = format!("{stem}_{book_id}{ext}");
    let mut n = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{stem}_{book_id}_{n}{ext}");
        n += 1;
    }
    candidate
}

/// Blocking archive writer; runs inside `spawn_blocking`.
fn write_archive(
    path: &Path,
    mut rx: mpsc::Receiver<WriterItem>,
) -> Result<WriteSummary, Box<dyn std::error::Error + Send + Sync>> {
    let output = BufWriter::new(File::create(path)?);
    let mut archive = zip::ZipWriter::new(output);

    let options: FileOptions<()> = FileOptions::default()
        .compression_level(Some(COMPRESSION_LEVEL))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut used = HashSet::new();
    let mut files = 0;

    while let Some(mut item) = rx.blocking_recv() {
        let name = unique_name(&mut used, &item.filename, item.book_id);
        archive.start_file(name, options)?;
        std::io::copy(&mut item.file, &mut archive)?;
        files += 1;
    }

    let mut inner = archive.finish()?;
    inner.flush()?;
    inner.into_inner().map_err(|e| e.into_error())?.sync_all()?;

    Ok(WriteSummary { files })
}

/// Downloads books (bounded parallelism, results kept in request order) and
/// hands them over to a blocking writer thread that builds the zip at
/// `output`.
pub async fn create_archive(
    ctx: &TaskCtx,
    books: Vec<Book>,
    file_format: SmartString,
    user_id: Option<i64>,
    normalized: bool,
    output: &Path,
) -> Result<ArchiveSummary, Box<dyn std::error::Error + Send + Sync>> {
    let (tx, rx) = mpsc::channel::<WriterItem>(2);
    let path = output.to_path_buf();
    let writer = tokio::task::spawn_blocking(move || write_archive(&path, rx));

    let total = books.len();
    let mut skipped = 0usize;
    let mut done = 0usize;
    let mut fatal: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    let mut writer_gone = false;

    {
        let ids: Vec<u64> = books.iter().map(|book| book.id).collect();
        let mut stream = futures_util::stream::iter(ids.into_iter().map(|book_id| {
            let file_format = file_format.clone();
            async move {
                (
                    book_id,
                    download(book_id, file_format, user_id, normalized).await,
                )
            }
        }))
        .buffered(DOWNLOAD_CONCURRENCY);

        while let Some((book_id, result)) = stream.next().await {
            done += 1;

            match result {
                Ok((file, filename)) => {
                    let item = WriterItem {
                        book_id,
                        file,
                        filename,
                    };
                    if tx.send(item).await.is_err() {
                        writer_gone = true;
                        break;
                    }
                }
                Err(err) => {
                    // Rate limiting (after retries) aborts the whole task;
                    // other errors skip just this book (and are counted).
                    if is_rate_limited(&*err) {
                        fatal = Some(err);
                        break;
                    }
                    log::warn!("Skipping book {} due to error: {}", book_id, err);
                    skipped += 1;
                }
            }

            let mut description = format!("Загрузка книг: {done}/{total}");
            if skipped > 0 {
                description.push_str(&format!(" (пропущено: {skipped})"));
            }
            set_progress_description(ctx, description).await;
        }
    }

    drop(tx);
    let summary = writer
        .await
        .map_err(|e| format!("archive writer task failed: {e}"))??;

    if let Some(err) = fatal {
        return Err(err);
    }
    if writer_gone {
        return Err("archive writer stopped unexpectedly".into());
    }

    Ok(ArchiveSummary {
        written: summary.files,
        skipped,
        total,
    })
}

/// The actual task body: list books, build archive, publish the result.
pub async fn run_archive_task(ctx: TaskCtx, data: CreateTask) {
    let token = ctx.token.clone();

    set_progress_description(&ctx, "Получение списка книг…".to_string()).await;

    let books = match data.object_type {
        ObjectType::Sequence => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_sequence_books,
                data.file_format.clone(),
            )
            .await
        }
        ObjectType::Author => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_author_books,
                data.file_format.clone(),
            )
            .await
        }
        ObjectType::Translator => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_translator_books,
                data.file_format.clone(),
            )
            .await
        }
    };

    let books = match books {
        Ok(v) => v,
        Err(err) => {
            set_task_error(token, "Failed getting books!".to_string()).await;
            log::error!("{}", err);
            return;
        }
    };

    if books.is_empty() {
        set_task_error(token, "No books!".to_string()).await;
        return;
    }

    let final_filename = match get_filename(
        data.object_type,
        data.object_id,
        data.file_format.clone(),
        data.normalized,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            set_task_error(token, "Can't get archive name!".to_string()).await;
            log::error!("{}", err);
            return;
        }
    };

    set_progress_description(&ctx, "Сборка архива...".to_string()).await;

    let part = partial_archive_path(&token);
    let target = archive_path(&token);
    let mut guard = FileGuard::new(vec![part.clone(), target.clone()]);

    let summary = match create_archive(
        &ctx,
        books,
        data.file_format,
        data.user_id,
        data.normalized,
        &part,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            set_task_error(token, "Failed downloading books!".to_string()).await;
            log::error!("{}", err);
            return;
        }
    };

    if summary.written == 0 {
        set_task_error(token, "All books failed to download".to_string()).await;
        return;
    }

    if let Err(err) = tokio::fs::rename(&part, &target).await {
        set_task_error(token, "Failed saving archive!".to_string()).await;
        log::error!("{}", err);
        return;
    }

    let size = match tokio::fs::metadata(&target).await {
        Ok(v) => v.len(),
        Err(err) => {
            set_task_error(token, "Failed getting archive metadata!".to_string()).await;
            log::error!("{}", err);
            return;
        }
    };

    let status_description = if summary.skipped > 0 {
        format!(
            "Архив готов! Ожидайте файл. Пропущено книг: {} из {}",
            summary.skipped, summary.total
        )
    } else {
        "Архив готов! Ожидайте файл".to_string()
    };

    put_task(Task {
        id: token,
        status: TaskStatus::Complete,
        status_description,
        error_message: None,
        result_filename: Some(final_filename),
        content_size: Some(size),
    })
    .await;

    guard.disarm();
}

#[derive(Debug, PartialEq)]
enum LimitReason {
    Idle,
    MaxDuration,
}

/// Drives `fut` until it finishes, failing it when there was no progress for
/// `idle` or when `max_total` has elapsed.
async fn run_with_limits<F: Future<Output = ()>>(
    ctx: &TaskCtx,
    fut: F,
    idle: Duration,
    max_total: Duration,
    check_every: Duration,
) -> Result<(), LimitReason> {
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(check_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ctx.touch();

    loop {
        tokio::select! {
            _ = &mut fut => return Ok(()),
            _ = ticker.tick() => {
                if ctx.started.elapsed() > max_total {
                    return Err(LimitReason::MaxDuration);
                }
                if ctx.idle_for() > idle {
                    return Err(LimitReason::Idle);
                }
            }
        }
    }
}

/// Reuses a live task for an identical request, or registers a new one and
/// runs it (once a concurrency slot is free). Atomic w.r.t. concurrent calls.
pub async fn create_or_reuse_task<F, Fut>(
    data: CreateTask,
    runner: F,
) -> Result<Task, CreateTaskError>
where
    F: FnOnce(TaskCtx, CreateTask) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let dedup_key = get_key(data.clone());

    let _lock = CREATE_LOCK.lock().await;

    if let Some(token) = DEDUP_INDEX.get(&dedup_key).await {
        if let Some(task) = get_task(&token).await {
            if task.status != TaskStatus::Failed {
                return Ok(task);
            }
        }
    }

    let slots = SEMAPHORE.available_permits();
    let waiting = WAITING.load(Ordering::SeqCst);
    if waiting >= slots + CONFIG.max_queued_tasks {
        return Err(CreateTaskError::QueueFull);
    }

    let token = generate_token();
    let queued = waiting >= slots;

    let task = Task {
        id: token.clone(),
        status: TaskStatus::InProgress,
        status_description: if queued {
            "В очереди…"
        } else {
            "Подготовка"
        }
        .to_string(),
        error_message: None,
        result_filename: None,
        content_size: None,
    };

    put_task(task.clone()).await;
    DEDUP_INDEX.insert(dedup_key, token.clone()).await;

    WAITING.fetch_add(1, Ordering::SeqCst);

    tokio::spawn(async move {
        let permit = SEMAPHORE.clone().acquire_owned().await;
        WAITING.fetch_sub(1, Ordering::SeqCst);

        let ctx = TaskCtx::new(token.clone());
        let fut = runner(ctx.clone(), data);
        let limits_ctx = ctx.clone();

        // Separate spawn so a panic inside the task is observed (and turned
        // into a Failed status) instead of leaving it "in progress" forever.
        let handle = tokio::spawn(async move {
            run_with_limits(
                &limits_ctx,
                fut,
                IDLE_TIMEOUT,
                MAX_TASK_DURATION,
                WATCHDOG_INTERVAL,
            )
            .await
        });

        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                let msg = match reason {
                    LimitReason::Idle => "Task timed out (no progress)",
                    LimitReason::MaxDuration => "Task timed out",
                };
                let _ = tokio::fs::remove_file(partial_archive_path(&token)).await;
                set_task_error(token, msg.to_string()).await;
            }
            Err(err) => {
                log::error!("task panicked: {err}");
                let _ = tokio::fs::remove_file(partial_archive_path(&token)).await;
                set_task_error(token, "Internal error".to_string()).await;
            }
        }

        drop(permit);
    });

    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn unique_name_appends_book_id_on_collision() {
        let mut used = HashSet::new();
        assert_eq!(unique_name(&mut used, "a.fb2", 1), "a.fb2");
        assert_eq!(unique_name(&mut used, "a.fb2", 2), "a_2.fb2");
        assert_eq!(unique_name(&mut used, "a.fb2", 2), "a_2_2.fb2");
        assert_eq!(unique_name(&mut used, "noext", 3), "noext");
        assert_eq!(unique_name(&mut used, "noext", 4), "noext_4");
    }

    #[test]
    fn write_archive_keeps_colliding_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.zip");
        let (tx, rx) = mpsc::channel(4);

        for id in [1u64, 2] {
            let mut f = tempfile::tempfile().unwrap();
            write!(f, "book{id}").unwrap();
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).unwrap();
            tx.blocking_send(WriterItem {
                book_id: id,
                file: f,
                filename: "same.fb2".into(),
            })
            .unwrap();
        }
        drop(tx);

        let summary = write_archive(&out, rx).ok().unwrap();
        assert_eq!(summary.files, 2);

        let mut zip = zip::ZipArchive::new(File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), 2);
        let mut s = String::new();
        zip.by_name("same_2.fb2")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "book2");
    }

    #[test]
    fn guard_removes_files_unless_disarmed() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();

        drop(FileGuard::new(vec![a.clone()]));
        assert!(!a.exists());

        let mut g = FileGuard::new(vec![b.clone()]);
        g.disarm();
        drop(g);
        assert!(b.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_fails_idle_task_but_not_progressing_one() {
        let ctx = TaskCtx::new("t".into());
        let idle = run_with_limits(
            &ctx,
            std::future::pending::<()>(),
            Duration::from_secs(30),
            Duration::from_secs(1000),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(idle, Err(LimitReason::Idle));

        let ctx = TaskCtx::new("t2".into());
        let worker_ctx = ctx.clone();
        let ok = run_with_limits(
            &ctx,
            async move {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    worker_ctx.touch();
                }
            },
            Duration::from_secs(30),
            Duration::from_secs(1000),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(ok, Ok(()));

        let ctx = TaskCtx::new("t3".into());
        let c = ctx.clone();
        let capped = run_with_limits(
            &ctx,
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    c.touch();
                }
            },
            Duration::from_secs(30),
            Duration::from_secs(100),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(capped, Err(LimitReason::MaxDuration));
    }
}
