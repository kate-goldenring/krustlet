//! A simple abstraction layer over OS specific details on watching a filesystem. Due to a bug in
//! MacOS with sending an event on socket creation, we need to implement our own hacky watcher. To
//! keep it as clean as possible, this module abstracts those details away behind a `Stream`
//! implementation. A bug has been filed with Apple and we can remove this if/when the bug is fixed

use std::{
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use log::error;
#[cfg(not(target_os = "macos"))]
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use notify::{Event, Result as NotifyResult};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct FileSystemWatcher(UnboundedReceiver<NotifyResult<Event>>);

impl Stream for FileSystemWatcher {
    type Item = NotifyResult<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

// For Windows and Linux, just use notify. For Mac, use our hacky workaround
impl FileSystemWatcher {
    #[cfg(not(target_os = "macos"))]
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let (stream_tx, stream_rx) = unbounded_channel::<NotifyResult<Event>>();
        let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
            if let Err(e) = stream_tx.send(res) {
                error!("Unable to send inotify event into stream: {:?}", e)
            }
        })?;
        watcher.configure(Config::PreciseEvents(true))?;

        watcher.watch(path, RecursiveMode::NonRecursive)?;

        Ok(FileSystemWatcher(stream_rx))
    }

    #[cfg(target_os = "macos")]
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        Ok(FileSystemWatcher(mac::dir_watcher(path)))
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use super::*;
    use notify::event::{CreateKind, EventKind, RemoveKind};
    use notify::Error as NotifyError;
    use tokio::fs::DirEntry;
    use tokio::stream::StreamExt;
    use tokio::time::{self, Duration};

    const WAIT_TIME: u64 = 2;

    pub fn dir_watcher<P: AsRef<Path>>(dir: P) -> UnboundedReceiver<NotifyResult<Event>> {
        let (tx, rx) = unbounded_channel();
        let path = dir.as_ref().to_path_buf();
        tokio::spawn(async move {
            let mut path_cache: HashSet<PathBuf> = match get_dir_list(&path).await {
                Ok(set) => set,
                Err(e) => {
                    error!(
                        "Unable to refresh directory {}, will attempt again: {:?}",
                        path.display(),
                        e
                    );
                    HashSet::new()
                }
            };

            loop {
                let current_paths: HashSet<PathBuf> = match get_dir_list(&path).await {
                    Ok(set) => set,
                    Err(e) => {
                        error!(
                            "Unable to refresh directory {}, will attempt again: {:?}",
                            path.display(),
                            e
                        );
                        if let Err(e) = tx.send(Err(NotifyError::io(e))) {
                            error!("Unable to send error {:?} due to channel being closed", e.0);
                        }
                        continue;
                    }
                };

                // Do a difference between cached and current paths (current - cached) to detect set of creates
                handle_creates(tx.clone(), current_paths.difference(&path_cache).cloned());

                // Do a difference between cached and current paths (cached - current) to detect set of deletes
                handle_deletes(tx.clone(), path_cache.difference(&current_paths).cloned());

                // Now we can set current to cached
                path_cache = current_paths;

                time::delay_for(Duration::from_secs(WAIT_TIME)).await;
            }
        });
        rx
    }

    async fn get_dir_list(path: &PathBuf) -> Result<HashSet<PathBuf>, std::io::Error> {
        // What does this monstrosity do? Well, due to async and all the random streaming involved
        // this:
        // 1. Reads the directory as a stream
        // 2. Maps the stream to a Vec of entries and handles any errors
        // 3. Converts the entries to PathBufs and puts them in a HashSet
        tokio::fs::read_dir(path)
            .await?
            .collect::<Result<Vec<DirEntry>, _>>()
            .await
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| e.path())
                    .collect::<HashSet<PathBuf>>()
            })
    }

    fn handle_creates(
        tx: UnboundedSender<NotifyResult<Event>>,
        items: impl Iterator<Item = PathBuf>,
    ) {
        let paths: Vec<PathBuf> = items.collect();
        // If there were no paths, it means there weren't any new files, so return
        if paths.is_empty() {
            return;
        }
        let event = Event {
            kind: EventKind::Create(CreateKind::Any),
            paths,
            ..Default::default()
        };
        if let Err(e) = tx.send(Ok(event)) {
            // At this point there isn't much we can do as the channel is closed. So just log an
            // error
            error!(
                "Unable to send event {:?} due to the channel being closed",
                e.0
            );
        }
    }

    fn handle_deletes(
        tx: UnboundedSender<NotifyResult<Event>>,
        items: impl Iterator<Item = PathBuf>,
    ) {
        let paths: Vec<PathBuf> = items.collect();
        // If there were no paths, it means there weren't any files deleted, so return
        if paths.is_empty() {
            return;
        }
        let event = Event {
            kind: EventKind::Remove(RemoveKind::Any),
            paths,
            ..Default::default()
        };
        if let Err(e) = tx.send(Ok(event)) {
            // At this point there isn't much we can do as the channel is closed. So just log an
            // error
            error!(
                "Unable to send event {:?} due to the channel being closed",
                e.0
            );
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[tokio::test]
        async fn test_handle_deletes() {
            let (tx, mut rx) = unbounded_channel();
            let file1 = PathBuf::from("/foo/bar");
            let file2 = PathBuf::from("/bar/foo");

            handle_deletes(tx, vec![file1.clone(), file2.clone()].into_iter());
            let event = rx
                .recv()
                .await
                .expect("got None result, which means the channel was closed prematurely")
                .expect("Got error from watch");

            assert!(event.kind.is_remove(), "Event is not a delete type");
            assert!(event.paths.len() == 2, "Event should contain two paths");
            assert!(event.paths.contains(&file1), "Missing expected path");
            assert!(event.paths.contains(&file2), "Missing expected path");
        }

        #[tokio::test]
        async fn test_handle_creates() {
            let (tx, mut rx) = unbounded_channel();
            let file1 = PathBuf::from("/foo/bar");
            let file2 = PathBuf::from("/bar/foo");

            handle_creates(tx, vec![file1.clone(), file2.clone()].into_iter());
            let event = rx
                .recv()
                .await
                .expect("got None result, which means the channel was closed prematurely")
                .expect("Got error from watch");

            assert!(event.kind.is_create(), "Event is not a create type");
            assert!(event.paths.len() == 2, "Event should contain two paths");
            assert!(event.paths.contains(&file1), "Missing expected path");
            assert!(event.paths.contains(&file2), "Missing expected path");
        }

        #[tokio::test]
        async fn test_watcher() {
            let temp = tempfile::tempdir().expect("unable to set up temporary directory");

            // Create some "existing" files in the directory
            let first = tokio::fs::write(temp.path().join("old_foo.txt"), "");
            let second = tokio::fs::write(temp.path().join("old_bar.txt"), "");

            tokio::try_join!(first, second).expect("unable to write test files");

            let mut rx = dir_watcher(&temp);

            let base = temp.path().to_owned();
            tokio::spawn(create_files(base));

            let event = tokio::time::timeout(Duration::from_secs(WAIT_TIME + 1), rx.recv())
                .await
                .expect("Timed out waiting for event")
                .expect("got None result, which means the channel was closed prematurely")
                .expect("Got error from watch");

            let mut found_create = false;
            let mut found_delete = false;

            assert_event(event, &temp, &mut found_create, &mut found_delete);

            let event = tokio::time::timeout(Duration::from_secs(WAIT_TIME + 1), rx.recv())
                .await
                .expect("Timed out waiting for event")
                .expect("got None result, which means the channel was closed prematurely")
                .expect("Got error from watch");

            assert_event(event, &temp, &mut found_create, &mut found_delete);

            // We should only get two different events, so this is just waiting for 1 second longer
            // than the loop to make sure we don't get another event
            assert!(
                tokio::time::timeout(Duration::from_secs(WAIT_TIME + 1), rx.recv())
                    .await
                    .is_err(),
                "Should not have gotten another event"
            );
        }

        async fn create_files(base: PathBuf) {
            // Wait for a bit to make sure things are started
            tokio::time::delay_for(Duration::from_secs(1)).await;
            let first = tokio::fs::write(base.join("new_foo.txt"), "");
            let second = tokio::fs::write(base.join("new_bar.txt"), "");
            let third = tokio::fs::remove_file(base.join("old_foo.txt"));

            tokio::try_join!(first, second, third).expect("unable to write/delete test files");
        }

        fn assert_event(
            event: Event,
            base: impl AsRef<Path>,
            found_create: &mut bool,
            found_delete: &mut bool,
        ) {
            match event.kind {
                EventKind::Create(_) => {
                    // Check if we already got a create event
                    if *found_create {
                        panic!("Got second create event");
                    }
                    assert!(event.paths.len() == 2, "Expected two created paths");
                    assert!(event.paths.contains(&base.as_ref().join("new_foo.txt")));
                    assert!(event.paths.contains(&base.as_ref().join("new_bar.txt")));
                    *found_create = true;
                }
                EventKind::Remove(_) => {
                    // Check if we already got a delete event
                    if *found_delete {
                        panic!("Got second delete event");
                    }
                    assert!(event.paths.len() == 1, "Expected 1 deleted path");
                    assert!(event.paths.contains(&base.as_ref().join("old_foo.txt")));
                    *found_delete = true;
                }
                _ => panic!("Event wasn't a create or remove"),
            }
        }
    }
}
