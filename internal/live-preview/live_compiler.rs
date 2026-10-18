// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use crate::file_watcher::FileWatcher;
use slint_interpreter::{Compiler, SendableCompilationResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// Builds a `.slint` file on a background thread, and builds it again when one of the files
/// that the last build read changes.
///
/// A burst of changes, like an editor saving several files, makes a single build.
/// Dropping the `LiveCompiler` stops the watching and the thread.
pub struct LiveCompiler {
    watched: Arc<Mutex<Watched>>,
    _changes: Arc<Changes>,
}

struct Watched {
    watcher: Option<FileWatcher>,
    build_paths: Vec<PathBuf>,
    extra_paths: Vec<PathBuf>,
}

impl Watched {
    fn update(&mut self) {
        let Some(watcher) = &mut self.watcher else { return };
        let paths = self.build_paths.iter().chain(&self.extra_paths).cloned();
        if let Err(err) = watcher.update_watched_paths(paths) {
            eprintln!("Warning: error while updating file watcher paths: {err:?}");
        }
    }

    fn set_build_paths(&mut self, result: &SendableCompilationResult) {
        let build_paths: Vec<PathBuf> = result
            .watch_paths(i_slint_core::InternalToken)
            .iter()
            .filter_map(i_slint_compiler::source_path::SourcePath::as_native_path)
            .map(std::path::Path::to_path_buf)
            .collect();
        if build_paths != self.build_paths {
            self.build_paths = build_paths;
            self.update();
        }
    }
}

/// What the file watcher tells the compile thread, apart from `Watched`:
/// the watcher calls back while it updates its paths.
struct Changes {
    request: mpsc::Sender<u64>,
    /// Bumped on every change, so that the thread builds once for a burst of changes.
    generation: AtomicU64,
}

impl LiveCompiler {
    /// Builds `path` on a background thread with a compiler from `compiler_factory`,
    /// and returns that first build.
    ///
    /// The thread builds `path` again after a change, and passes each of these builds to `on_rebuild`.
    pub fn start(
        compiler_factory: impl FnOnce() -> Compiler + Send + 'static,
        path: PathBuf,
        mut on_rebuild: impl FnMut(SendableCompilationResult) + Send + 'static,
    ) -> Result<(Self, SendableCompilationResult), String> {
        let (request, requests) = mpsc::channel();
        let changes = Arc::new(Changes { request, generation: AtomicU64::new(0) });
        let changes_weak = Arc::downgrade(&changes);
        let watcher = FileWatcher::start(
            move |_event| {
                let Some(changes) = changes_weak.upgrade() else { return };
                let generation = changes.generation.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = changes.request.send(generation);
            },
            move |err| eprintln!("Warning: file watcher error: {err}"),
        )
        .map_err(|err| eprintln!("Warning: cannot watch the files of {}: {err}", path.display()))
        .ok();
        let watched = Arc::new(Mutex::new(Watched {
            watcher,
            build_paths: Vec::new(),
            extra_paths: Vec::new(),
        }));

        let (first_build, first_build_rx) = mpsc::channel();
        let watched_weak = Arc::downgrade(&watched);
        let changes_weak = Arc::downgrade(&changes);
        std::thread::Builder::new()
            .name("slint-live-preview-compiler".into())
            .spawn(move || {
                let mut compiler = compiler_factory();
                compiler
                    .set_embed_resources(i_slint_compiler::EmbedResourcesKind::ListAllResources);
                let build = || -> Option<SendableCompilationResult> {
                    let result = poll_ready(compiler.build_from_path(&path)).into_send();
                    watched_weak.upgrade()?.lock().unwrap().set_build_paths(&result);
                    Some(result)
                };

                let Some(result) = build() else { return };
                if first_build.send(result).is_err() {
                    return;
                }
                while let Ok(mut generation) = requests.recv() {
                    std::thread::sleep(crate::REBUILD_DEBOUNCE);
                    while let Ok(newer) = requests.try_recv() {
                        generation = newer;
                    }
                    let Some(result) = build() else { return };
                    let Some(changes) = changes_weak.upgrade() else { return };
                    // A newer change arrived while compiling; the next request is fresher
                    if changes.generation.load(Ordering::Relaxed) == generation {
                        on_rebuild(result);
                    }
                }
            })
            .map_err(|err| format!("Cannot start the live-preview compile thread: {err}"))?;

        let first = first_build_rx.recv().map_err(|_| "The compile thread stopped".to_string())?;
        Ok((Self { watched, _changes: changes }, first))
    }

    /// Watches `paths` too, in addition to the files that the builds read.
    pub fn set_extra_watch_paths(&self, paths: Vec<PathBuf>) {
        let mut watched = self.watched.lock().unwrap();
        watched.extra_paths = paths;
        watched.update();
    }
}

/// Poll a future that is expected to be immediately ready. The compiler is only
/// asynchronous when an async file loader is set, which the live preview does
/// not use.
pub fn poll_ready<F: std::future::Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match std::future::Future::poll(future.as_mut(), &mut cx) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => unreachable!("Compiler returned Pending"),
    }
}
