//! 任务：在独立线程里运行，可取消、可暂停（任务自己在检查点等待）、进度节流；panic 在任务边界捕获并转成失败。

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use yisi_engine::protocol::{
    CANCELLED, COMPLETED, FAILED, JobCompleted, JobFailed, JobRef, PROGRESS, Progress,
};

use crate::runtime::Outbox;

/// 两次进度通知之间至少隔这么久，即每秒最多 10 次。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

pub enum JobOutcome {
    Completed(Value),
    Cancelled,
}

pub struct RunningJob {
    id: String,
    cancel: Arc<AtomicBool>,
    pause: Arc<PauseGate>,
    pausable: bool,
}

impl RunningJob {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 请求取消；任务在下一个检查点结束。暂停中的任务也会被放行，好让它看到取消。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause.wake();
    }

    /// 标明这个任务会在检查点调用 [`JobContext::wait_while_paused`]，可以暂停。
    pub fn pausable(mut self) -> Self {
        self.pausable = true;
        self
    }

    pub fn is_pausable(&self) -> bool {
        self.pausable
    }

    /// 暂停或继续；任务在下一个检查点停下，继续后接着做。
    pub fn set_paused(&self, paused: bool) {
        let mut state = self.pause.lock();
        state.paused = paused;
        drop(state);
        self.pause.changed.notify_all();
    }
}

/// 暂停闸门：暂停时任务在检查点等着，继续或取消时放行；记下累计暂停了多久，报告用时时扣掉。
#[derive(Default)]
struct PauseGate {
    state: Mutex<PauseState>,
    changed: Condvar,
}

#[derive(Default)]
struct PauseState {
    paused: bool,
    paused_total: Duration,
}

impl PauseGate {
    fn lock(&self) -> MutexGuard<'_, PauseState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 先拿锁再通知：等待方在锁内查过取消标志才睡下，这样不会漏掉这次唤醒。
    fn wake(&self) {
        drop(self.lock());
        self.changed.notify_all();
    }
}

pub struct JobContext {
    id: String,
    outbox: Arc<Outbox>,
    cancel: Arc<AtomicBool>,
    pause: Arc<PauseGate>,
    progress: ProgressSender,
}

/// 一个任务的进度通知，节流到每秒最多 10 次。浏览任务的扫描在会话的线程里报进度，也用它。
pub struct ProgressSender {
    id: String,
    outbox: Arc<Outbox>,
    last: Mutex<Option<Instant>>,
}

impl ProgressSender {
    pub fn new(id: String, outbox: Arc<Outbox>) -> Self {
        Self {
            id,
            outbox,
            last: Mutex::new(None),
        }
    }

    /// 距上次不足 [`PROGRESS_INTERVAL`] 时丢弃，但完成那一次（`done == total`）总会发出。
    pub fn send(&self, stage: Option<&str>, done: u64, total: u64) {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let due = last.is_none_or(|sent| now.duration_since(sent) >= PROGRESS_INTERVAL);
        if !due && done != total {
            return;
        }
        *last = Some(now);
        self.outbox.notify(
            PROGRESS,
            Progress {
                job_id: self.id.clone(),
                done,
                total,
                stage: stage.map(str::to_owned),
            },
        );
    }
}

impl JobContext {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 发一条任务通知（参数里自行带上任务 ID）。
    pub fn notify(&self, method: &str, params: impl Serialize) {
        self.outbox.notify(method, params);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 暂停中就在这里等，直到继续或取消；没暂停时立即返回。
    pub fn wait_while_paused(&self) {
        let mut state = self.pause.lock();
        if !state.paused {
            return;
        }
        let since = Instant::now();
        while state.paused && !self.is_cancelled() {
            state = self
                .pause
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.paused_total += since.elapsed();
    }

    /// 到目前为止累计暂停的时长。
    pub fn paused_total(&self) -> Duration {
        self.pause.lock().paused_total
    }

    /// 报告进度。距上次不足 [`PROGRESS_INTERVAL`] 时丢弃，但完成那一次（`done == total`）总会发出。
    pub fn progress(&self, done: u64, total: u64) {
        self.progress.send(None, done, total);
    }

    /// 报告任务里某个阶段的进度，规则同 [`progress`](Self::progress)。
    pub fn stage_progress(&self, stage: &str, done: u64, total: u64) {
        self.progress.send(Some(stage), done, total);
    }
}

/// 在新线程里运行任务，结束时发出 completed / cancelled / failed 之一，再调用 `on_finished`。
pub fn start<W, D>(id: String, outbox: Arc<Outbox>, work: W, on_finished: D) -> RunningJob
where
    W: FnOnce(&JobContext) -> Result<JobOutcome, String> + Send + 'static,
    D: FnOnce() + Send + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(PauseGate::default());
    let context = JobContext {
        id: id.clone(),
        progress: ProgressSender::new(id.clone(), Arc::clone(&outbox)),
        outbox,
        cancel: Arc::clone(&cancel),
        pause: Arc::clone(&pause),
    };
    thread::spawn(move || {
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| work(&context)));
        let job_id = context.id.clone();
        match outcome {
            Ok(Ok(JobOutcome::Completed(result))) => context
                .outbox
                .notify(COMPLETED, JobCompleted { job_id, result }),
            Ok(Ok(JobOutcome::Cancelled)) => context.outbox.notify(CANCELLED, JobRef { job_id }),
            Ok(Err(message)) => context.outbox.notify(FAILED, JobFailed { job_id, message }),
            Err(payload) => context.outbox.notify(
                FAILED,
                JobFailed {
                    job_id,
                    message: format!("引擎内部错误：{}", panic_message(payload.as_ref())),
                },
            ),
        }
        on_finished();
    });
    RunningJob {
        id,
        cancel,
        pause,
        pausable: false,
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知原因".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::mpsc;

    use yisi_rpc::{Message, read_message};

    use super::*;

    /// 把发给宿主的字节收集起来，供测试检查。
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuffer {
        fn messages(&self) -> Vec<Message> {
            let bytes = self.0.lock().unwrap().clone();
            let mut reader = Cursor::new(bytes);
            let mut messages = Vec::new();
            while let Ok(message) = read_message(&mut reader) {
                messages.push(message);
            }
            messages
        }
    }

    fn run_to_end(
        work: impl FnOnce(&JobContext) -> Result<JobOutcome, String> + Send + 'static,
    ) -> Vec<Message> {
        let buffer = SharedBuffer::default();
        let outbox = Arc::new(Outbox::new(buffer.clone()));
        let (done, finished) = mpsc::channel();
        start("j1".to_owned(), outbox, work, move || {
            done.send(()).unwrap()
        });
        finished.recv_timeout(Duration::from_secs(10)).unwrap();
        buffer.messages()
    }

    fn last_notification(messages: &[Message]) -> (String, Value) {
        match messages.last() {
            Some(Message::Notification(notification)) => {
                (notification.method.clone(), notification.params.clone())
            }
            other => panic!("最后一条应为通知，实际为 {other:?}"),
        }
    }

    #[test]
    fn panic_inside_job_becomes_failed_notification() {
        let messages = run_to_end(|_| panic!("模拟的解析器缺陷"));
        let (method, params) = last_notification(&messages);
        assert_eq!(method, FAILED);
        assert_eq!(params["jobId"], "j1");
        assert!(
            params["message"]
                .as_str()
                .unwrap()
                .contains("模拟的解析器缺陷")
        );
    }

    #[test]
    fn outcomes_map_to_terminal_notifications() {
        let (method, params) =
            last_notification(&run_to_end(|_| Ok(JobOutcome::Completed(Value::from(5)))));
        assert_eq!(
            (method.as_str(), &params["result"]),
            (COMPLETED, &Value::from(5))
        );

        let (method, _) = last_notification(&run_to_end(|_| Ok(JobOutcome::Cancelled)));
        assert_eq!(method, CANCELLED);

        let (method, params) = last_notification(&run_to_end(|_| Err("打不开".to_owned())));
        assert_eq!(
            (method.as_str(), &params["message"]),
            (FAILED, &Value::from("打不开"))
        );
    }

    /// 任务每轮过一次暂停闸门，把轮数发回来；`stop` 置位后结束。
    fn start_counting_job() -> (RunningJob, mpsc::Receiver<u32>, Arc<AtomicBool>) {
        let outbox = Arc::new(Outbox::new(SharedBuffer::default()));
        let (rounds, received) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::clone(&stop);
        let running = start(
            "j1".to_owned(),
            outbox,
            move |context| {
                let mut round = 0;
                while !should_stop.load(Ordering::Relaxed) {
                    context.wait_while_paused();
                    if context.is_cancelled() {
                        return Ok(JobOutcome::Cancelled);
                    }
                    round += 1;
                    let _ = rounds.send(round);
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(JobOutcome::Completed(Value::Null))
            },
            || {},
        );
        (running, received, stop)
    }

    #[test]
    fn paused_job_waits_until_resumed() {
        let (running, rounds, stop) = start_counting_job();
        rounds.recv_timeout(Duration::from_secs(5)).unwrap();
        running.set_paused(true);
        // 已经过了闸门的那一轮可能还会发出来，之后就停住
        thread::sleep(Duration::from_millis(50));
        while rounds.try_recv().is_ok() {}
        assert!(
            rounds.recv_timeout(Duration::from_millis(200)).is_err(),
            "暂停后不应再有新的一轮"
        );
        running.set_paused(false);
        rounds.recv_timeout(Duration::from_secs(5)).unwrap();
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn cancel_releases_a_paused_job() {
        let (running, rounds, _stop) = start_counting_job();
        rounds.recv_timeout(Duration::from_secs(5)).unwrap();
        running.set_paused(true);
        thread::sleep(Duration::from_millis(50));
        while rounds.try_recv().is_ok() {}
        running.cancel();
        // 任务退出后发送端被丢弃，通道断开；还停在闸门上的话这里会超时
        match rounds.recv_timeout(Duration::from_secs(5)) {
            Ok(_) => panic!("取消后不应再有新的一轮"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("取消没能放行暂停中的任务"),
        }
    }

    #[test]
    fn progress_is_throttled_but_final_progress_always_sent() {
        let messages = run_to_end(|context| {
            for done in 1..=1000 {
                context.progress(done, 1000);
            }
            Ok(JobOutcome::Completed(Value::Null))
        });
        let progress: Vec<u64> = messages
            .iter()
            .filter_map(|message| match message {
                Message::Notification(notification) if notification.method == PROGRESS => {
                    notification.params["done"].as_u64()
                }
                _ => None,
            })
            .collect();
        assert_eq!(progress.first(), Some(&1));
        assert_eq!(progress.last(), Some(&1000));
        assert!(progress.len() < 20, "节流后仍有 {} 条进度", progress.len());
    }
}
