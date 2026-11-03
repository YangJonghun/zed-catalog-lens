use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use tokio::task::JoinHandle;
use tower_lsp_server::ls_types::{
    DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher, GlobPattern, Registration,
};

const GLOBS: [&str; 3] = [
    "**/pnpm-workspace.yaml",
    "**/.yarnrc.yml",
    "**/package.json",
];

pub fn registration() -> Registration {
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: GLOBS
            .iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String((*glob).to_owned()),
                kind: None,
            })
            .collect(),
    };
    Registration {
        id: "catalog-lens-workspace-files".to_owned(),
        method: "workspace/didChangeWatchedFiles".to_owned(),
        register_options: serde_json::to_value(options).ok(),
    }
}

pub fn is_relevant<'a>(changed: &Path, open_documents: impl IntoIterator<Item = &'a Path>) -> bool {
    let Some(changed_dir) = changed.parent() else {
        return false;
    };
    open_documents
        .into_iter()
        .filter_map(Path::parent)
        .any(|doc_dir| doc_dir.starts_with(changed_dir))
}

pub struct Debouncer {
    delay: Duration,
    pending: Mutex<Option<JoinHandle<()>>>,
}

impl Debouncer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: Mutex::new(None),
        }
    }

    pub fn schedule(&self, work: impl Future<Output = ()> + Send + 'static) {
        let delay = self.delay;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            work.await;
        });
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = pending.replace(handle) {
            previous.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn registration_covers_the_three_definition_files() {
        let registration = registration();
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        let options = registration.register_options.unwrap();
        let globs: Vec<&str> = options["watchers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["globPattern"].as_str().unwrap())
            .collect();
        assert_eq!(globs, GLOBS);
    }

    #[test]
    fn only_ancestor_changes_are_relevant() {
        let open = [PathBuf::from("/ws/packages/app/package.json")];
        let docs = || open.iter().map(PathBuf::as_path);
        assert!(is_relevant(Path::new("/ws/pnpm-workspace.yaml"), docs()));
        assert!(is_relevant(
            Path::new("/ws/packages/app/package.json"),
            docs()
        ));
        assert!(!is_relevant(
            Path::new("/ws/packages/other/package.json"),
            docs()
        ));
        assert!(!is_relevant(
            Path::new("/elsewhere/pnpm-workspace.yaml"),
            docs()
        ));
    }

    #[tokio::test]
    async fn debouncer_runs_only_the_last_scheduled_work() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let debouncer = Debouncer::new(Duration::from_millis(20));
        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let runs = runs.clone();
            debouncer.schedule(async move {
                runs.fetch_add(1, Ordering::SeqCst);
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
