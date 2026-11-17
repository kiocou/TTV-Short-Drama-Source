//! 按产物共享执行体；请求只持有订阅，最后一个订阅离开才取消底层工作。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const CANCELLED: &str = "媒体请求已取消。";

#[derive(Clone)]
pub struct Cancellation(watch::Sender<bool>);

impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn run<T>(&self, work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(CANCELLED.to_owned()),
            result = work => result,
        }
    }
}

struct Entry<T> {
    result: watch::Sender<Option<Result<T, String>>>,
    cancel: Cancellation,
    subscribers: AtomicUsize,
}

pub struct Registry<T>(Arc<Mutex<HashMap<String, Arc<Entry<T>>>>>);

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl<T: Clone + Send + Sync + 'static> Registry<T> {
    pub fn subscribe<F, Fut>(&self, key: String, work: F) -> Subscription<T>
    where
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send + 'static,
    {
        let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = entries
            .get(&key)
            .filter(|entry| !entry.cancel.is_cancelled())
        {
            #[allow(deprecated)]
            let incremented = entry
                .subscribers
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count > 0).then_some(count + 1)
                })
                .is_ok();
            if incremented {
                return Subscription(Arc::clone(entry));
            }
        }
        // 取消后的立即重试要等旧执行体析构，不能与它同时写同一产物。
        let previous = entries.get(&key).map(|entry| entry.result.subscribe());
        let entry = Arc::new(Entry {
            result: watch::channel(None).0,
            cancel: Cancellation::default(),
            subscribers: AtomicUsize::new(1),
        });
        entries.insert(key.clone(), Arc::clone(&entry));
        let completion = Completion {
            entries: Arc::clone(&self.0),
            key,
            entry: Arc::clone(&entry),
        };
        let cancel = entry.cancel.clone();
        tokio::spawn(async move {
            if let Some(mut previous) = previous {
                while previous.borrow_and_update().is_none() {
                    if previous.changed().await.is_err() {
                        break;
                    }
                }
            }
            let result = cancel.run(work(cancel.clone())).await;
            completion.complete(result);
        });
        Subscription(entry)
    }
}

struct Completion<T> {
    entries: Arc<Mutex<HashMap<String, Arc<Entry<T>>>>>,
    key: String,
    entry: Arc<Entry<T>>,
}

impl<T> Completion<T> {
    fn complete(self, result: Result<T, String>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            entries.remove(&self.key);
        }
        self.entry.result.send_replace(Some(result));
    }
}

impl<T> Drop for Completion<T> {
    fn drop(&mut self) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            entries.remove(&self.key);
        }
        if self.entry.result.borrow().is_none() {
            self.entry
                .result
                .send_replace(Some(Err("媒体执行体异常退出，请重试。".to_owned())));
        }
    }
}

pub struct Subscription<T>(Arc<Entry<T>>);

impl<T> Clone for Subscription<T> {
    fn clone(&self) -> Self {
        self.0.subscribers.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(&self.0))
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        if self.0.subscribers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.cancel.cancel();
        }
    }
}

impl<T: Clone> Subscription<T> {
    pub fn is_finished(&self) -> bool {
        self.0.result.borrow().is_some()
    }

    pub fn same_task(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn subscriber_count(&self) -> usize {
        self.0.subscribers.load(Ordering::Acquire)
    }

    pub fn completion(&self) -> impl Future<Output = ()> + Send + 'static
    where
        T: Send + Sync + 'static,
    {
        let mut receiver = self.0.result.subscribe();
        async move {
            while receiver.borrow_and_update().is_none() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub async fn wait(&self) -> Result<T, String> {
        let mut receiver = self.0.result.subscribe();
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| "媒体结果通道已关闭。".to_owned())?;
        }
    }
}

enum RequestState {
    Active(Cancellation),
    Cancelled(Instant),
}

#[derive(Default)]
pub struct Requests(Arc<Mutex<HashMap<String, RequestState>>>);

pub struct Request {
    requests: Arc<Mutex<HashMap<String, RequestState>>>,
    key: Option<String>,
    pub cancel: Cancellation,
}

impl Requests {
    pub fn begin(&self, key: Option<String>) -> Result<Request, String> {
        let cancel = Cancellation::default();
        if let Some(key) = &key {
            if key.trim().is_empty() || key.len() > 160 {
                return Err("无效的媒体请求编号。".to_owned());
            }
            let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
            match entries.remove(key) {
                Some(RequestState::Active(previous)) => {
                    entries.insert(key.clone(), RequestState::Active(previous));
                    return Err("媒体请求编号正在使用。".to_owned());
                }
                Some(RequestState::Cancelled(at)) if at.elapsed() < Duration::from_secs(120) => {
                    cancel.cancel()
                }
                _ => {}
            }
            entries.insert(key.clone(), RequestState::Active(cancel.clone()));
        }
        Ok(Request {
            requests: Arc::clone(&self.0),
            key,
            cancel,
        })
    }

    pub fn cancel(&self, key: &str) {
        if key.is_empty() || key.len() > 160 {
            return;
        }
        let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(RequestState::Active(token)) = entries.get(key) {
            token.cancel();
            return;
        }
        entries.retain(|_, state| !matches!(state, RequestState::Cancelled(at) if at.elapsed() >= Duration::from_secs(120)));
        // cancel 可以先于 invoke 到达；墓碑有时限和容量上限，避免未知编号无限占内存。
        if entries
            .values()
            .filter(|state| matches!(state, RequestState::Cancelled(_)))
            .count()
            >= 1024
        {
            let oldest = entries
                .iter()
                .filter_map(|(key, state)| match state {
                    RequestState::Cancelled(at) => Some((key.clone(), *at)),
                    _ => None,
                })
                .min_by_key(|(_, at)| *at)
                .map(|(key, _)| key);
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(key.to_owned(), RequestState::Cancelled(Instant::now()));
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(key) = &self.key {
            self.requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(key);
        }
    }
}

/// stdout/stderr 读取任务随调用作用域退出，避免取消后仍留下独立 Tokio 任务。
pub struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 只接管本次启动的 worker 及其子进程；关闭作用域时连同它启动的 ffmpeg 一起回收。
#[cfg(windows)]
pub struct WorkerProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for WorkerProcessTree {}

#[cfg(windows)]
impl WorkerProcessTree {
    fn attach(child: &tokio::process::Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(format!(
                    "创建媒体进程组失败：{}",
                    std::io::Error::last_os_error()
                ));
            }
            let job = Self(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let process = child
                .raw_handle()
                .ok_or_else(|| "媒体 worker 已退出。".to_owned())?;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, process as _) == 0
            {
                return Err(format!(
                    "绑定媒体进程组失败：{}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(job)
        }
    }
}

/// 挂起创建保证 worker 在加入进程组之前没有机会启动逃逸的 ffmpeg 子进程。
pub fn spawn_worker(
    command: &mut tokio::process::Command,
) -> Result<(tokio::process::Child, WorkerProcessTree), String> {
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0004);
    let child = command
        .spawn()
        .map_err(|error| format!("启动媒体 worker 失败：{error}"))?;
    #[cfg(windows)]
    {
        #[link(name = "ntdll")]
        extern "system" {
            fn NtResumeProcess(handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
        }
        let tree = WorkerProcessTree::attach(&child)?;
        let status = unsafe {
            NtResumeProcess(
                child
                    .raw_handle()
                    .ok_or_else(|| "媒体 worker 已退出。".to_owned())? as _,
            )
        };
        if status < 0 {
            return Err(format!("启动媒体进程组失败：NTSTATUS {status:#x}"));
        }
        Ok((child, tree))
    }
    #[cfg(not(windows))]
    Ok((child, WorkerProcessTree))
}

#[cfg(not(windows))]
pub struct WorkerProcessTree;

#[cfg(windows)]
impl Drop for WorkerProcessTree {
    fn drop(&mut self) {
        let wait_and_close = || unsafe {
            use windows_sys::Win32::System::JobObjects::*;
            if TerminateJobObject(self.0, 1) != 0 {
                // 终止调用只提出请求；旧子树归零后才能通知同产物的下一次执行。
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                    if QueryInformationJobObject(
                        self.0,
                        JobObjectBasicAccountingInformation,
                        &mut info as *mut _ as _,
                        std::mem::size_of_val(&info) as u32,
                        std::ptr::null_mut(),
                    ) == 0
                        || info.ActiveProcesses == 0
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        crate::trace::log("[媒体] 等待已终止进程组退出超时（5秒），结束本次清理");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        };
        if tokio::runtime::Handle::try_current().is_ok_and(|h| {
            matches!(
                h.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            )
        }) {
            tokio::task::block_in_place(wait_and_close);
        } else {
            wait_and_close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn settle() {
        for _ in 0..12 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn same_artifact_shares_one_execution_and_result() {
        let registry = Registry::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Cancellation::default();
        let acquire = || {
            let calls = Arc::clone(&calls);
            let gate = gate.clone();
            registry.subscribe("same".into(), move |_| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                gate.cancelled().await;
                Ok(7)
            })
        };
        let first = acquire();
        let second = acquire();
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "同一产物只能有一个执行体");
        gate.cancel();
        assert_eq!(first.wait().await, Ok(7));
        assert_eq!(second.wait().await, Ok(7));
        settle().await;
        assert!(registry.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn different_artifacts_execute_independently() {
        let registry = Registry::default();
        let first = registry.subscribe("a".into(), |_| async { Ok(1) });
        let second = registry.subscribe("b".into(), |_| async { Ok(2) });
        assert_eq!(first.wait().await, Ok(1));
        assert_eq!(second.wait().await, Ok(2));
    }

    #[tokio::test]
    async fn only_the_final_subscriber_cancels_the_execution() {
        let registry: Registry<()> = Registry::default();
        let first = registry.subscribe("cancel".into(), |_| std::future::pending());
        let second = first.clone();
        let token = first.0.cancel.clone();
        drop(first);
        assert!(!token.is_cancelled());
        drop(second);
        assert!(token.is_cancelled(), "最后一个订阅离开必须传递取消");
        settle().await;
        assert!(registry.0.lock().unwrap().is_empty());
        let retry = registry.subscribe("cancel".into(), |_| async { Ok(()) });
        assert_eq!(retry.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn failure_and_panic_release_the_artifact_for_retry() {
        let registry: Registry<()> = Registry::default();
        for panic in [false, true] {
            let failed = registry.subscribe("retry".into(), move |_| async move {
                if panic {
                    panic!("模拟执行体异常退出");
                }
                Err("模拟下载失败".into())
            });
            assert!(tokio::time::timeout(Duration::from_secs(1), failed.wait())
                .await
                .unwrap()
                .is_err());
            settle().await;
            assert!(registry.0.lock().unwrap().is_empty());
        }
        assert_eq!(
            registry
                .subscribe("retry".into(), |_| async { Ok(()) })
                .wait()
                .await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn cancelling_parent_releases_nested_stage_subscription() {
        let source: Arc<Registry<()>> = Arc::new(Registry::default());
        let parent: Registry<()> = Registry::default();
        let source_for_parent = Arc::clone(&source);
        let subscriber = parent.subscribe("download".into(), move |_| async move {
            source_for_parent
                .subscribe("signature".into(), |_| std::future::pending())
                .wait()
                .await
        });
        settle().await;
        assert_eq!(source.0.lock().unwrap().len(), 1);
        drop(subscriber);
        settle().await;
        assert!(
            source.0.lock().unwrap().is_empty(),
            "下载取消必须释放取流订阅"
        );
    }

    #[tokio::test]
    async fn cancel_before_registration_and_independent_requests() {
        let requests = Requests::default();
        requests.cancel("early");
        let early = requests.begin(Some("early".into())).unwrap();
        assert_eq!(
            early.cancel.run(async { Ok(1) }).await,
            Err(CANCELLED.into())
        );
        let first = requests.begin(Some("first".into())).unwrap();
        let second = requests.begin(Some("second".into())).unwrap();
        requests.cancel("first");
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled());
        drop(first);
        assert!(!requests
            .begin(Some("first".into()))
            .unwrap()
            .cancel
            .is_cancelled());
        assert!(requests.begin(Some("second".into())).is_err());
    }

    #[test]
    fn cancellation_tombstones_are_bounded() {
        let requests = Requests::default();
        for index in 0..1100 {
            requests.cancel(&format!("request-{index}"));
        }
        assert_eq!(requests.0.lock().unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn immediate_retry_waits_for_old_execution_and_preserves_new_entry() {
        struct Running(Arc<AtomicUsize>);
        impl Drop for Running {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let registry: Registry<()> = Registry::default();
        let running = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&running);
        let first = registry.subscribe("artifact".into(), move |_| async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let _running = Running(counter);
            std::future::pending().await
        });
        settle().await;
        drop(first);
        let gate = Cancellation::default();
        let wait_gate = gate.clone();
        let counter = Arc::clone(&running);
        let retry = registry.subscribe("artifact".into(), move |_| async move {
            assert_eq!(
                counter.fetch_add(1, Ordering::SeqCst),
                0,
                "旧执行体未释放前不能重试"
            );
            let _running = Running(counter);
            wait_gate.cancelled().await;
            Ok(())
        });
        settle().await;
        assert_eq!(
            registry.0.lock().unwrap().len(),
            1,
            "旧执行体清理不能移除新占位"
        );
        gate.cancel();
        retry.wait().await.unwrap();
    }
}
