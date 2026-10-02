//! Upkeep of the CEF cache directory: the browser profiles HTML sources keep
//! there, and the lock Chromium leaves behind in it.
//!
//! Every HTML source gets a profile directory of its own under the cache root
//! (see `html_input::profile_dir`). Nothing removes one when its block goes,
//! so on a Strom whose cache outlives the process - every native run - they
//! pile up, each holding the cookies of whatever page was logged in to.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use strom_types::{Flow, FlowId};
use tracing::{debug, info, warn};

use crate::blocks::builtin::html_input;

/// Name prefixes of the profile directories Strom creates. Anything else in
/// the cache root is Chromium's, or the fake media devices, and is left alone.
const BLOCK_PREFIX: &str = "strom-block-";
const ELEMENT_PREFIX: &str = "strom-element-";
const NAMED_PREFIX: &str = "strom-named-";

/// The CEF cache root this process uses, if it has one.
pub fn cache_root() -> Option<PathBuf> {
    std::env::var_os("GST_CEF_CACHE_LOCATION").map(PathBuf::from)
}

/// Every profile directory one of `flows` would use when it runs.
fn profiles_in_use<'a>(root: &Path, flows: impl IntoIterator<Item = &'a Flow>) -> HashSet<PathBuf> {
    let mut used = HashSet::new();
    for flow in flows {
        for block in &flow.blocks {
            if block.block_definition_id != html_input::BLOCK_ID {
                continue;
            }
            used.insert(html_input::stored_block_profile_dir(root, &flow.id, block));
        }
        for element in &flow.elements {
            // A flow that sets isolated-context itself gets no profile from Strom.
            if element.element_type == "cefsrc"
                && !element
                    .properties
                    .contains_key(html_input::ISOLATED_CONTEXT_PROPERTY)
            {
                used.insert(html_input::element_profile_dir(
                    root,
                    &flow.id.to_string(),
                    &element.id,
                ));
            }
        }
    }
    used
}

/// Remove every profile directory under `root` that none of `flows` uses.
///
/// Meant for startup, before any flow runs: a profile a running browser has
/// open is never in danger, because there is none. Named profiles go once no
/// block names them.
pub fn remove_unused_profiles<'a>(root: &Path, flows: impl IntoIterator<Item = &'a Flow>) {
    let used = profiles_in_use(root, flows);
    remove_profiles(
        root,
        &|name: &str, path: &Path| {
            (name.starts_with(BLOCK_PREFIX)
                || name.starts_with(ELEMENT_PREFIX)
                || name.starts_with(NAMED_PREFIX))
                && !used.contains(path)
        },
        true,
    );
}

/// Remove the profiles a deleted flow's blocks and elements had of their own.
///
/// Named profiles stay: another flow may share one, and the next startup
/// removes it if none does. A flow id is a UUID, fixed in length, so the
/// prefix cannot match another flow's directories.
///
/// Chromium goes on writing a profile for a moment after its browser has
/// closed - measured: removing it right after the flow stopped failed with
/// "Directory not empty" - so a profile that will not go yet is tried again
/// a few times, in the background.
pub fn remove_flow_profiles(root: &Path, flow_id: &FlowId) {
    let block = format!("{}{}-", BLOCK_PREFIX, flow_id);
    let element = format!("{}{}-", ELEMENT_PREFIX, flow_id);
    let doomed = move |name: &str, _: &Path| name.starts_with(&block) || name.starts_with(&element);
    let failed = remove_profiles(root, &doomed, false);
    if failed == 0 {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        warn!(
            "{} browser profile(s) of flow {} could not be removed yet; the next startup \
             removes them",
            failed, flow_id
        );
        return;
    };
    let root = root.to_path_buf();
    handle.spawn(async move {
        for attempt in 1..=PROFILE_RETRIES {
            tokio::time::sleep(PROFILE_RETRY_DELAY).await;
            let last = attempt == PROFILE_RETRIES;
            if remove_profiles(&root, &doomed, last) == 0 {
                return;
            }
        }
    });
}

/// How often, and how far apart, a profile Chromium is still writing is
/// tried again.
const PROFILE_RETRIES: u32 = 5;
const PROFILE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Remove every profile `doomed` picks, and return how many would not go.
/// A failure is only warned about when `last` says nothing will try again.
fn remove_profiles(root: &Path, doomed: &impl Fn(&str, &Path) -> bool, last: bool) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut failed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || !doomed(&name, &path) {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => info!("Removed unused browser profile {}", path.display()),
            Err(e) => {
                failed += 1;
                if last {
                    warn!(
                        "Could not remove unused browser profile {}: {} - the next startup \
                         removes it",
                        path.display(),
                        e
                    );
                } else {
                    debug!("Browser profile {} is still in use: {}", path.display(), e);
                }
            }
        }
    }
    failed
}

/// Clear a process lock Chromium would refuse to break.
///
/// Chromium locks its cache root with a `SingletonLock` symlink naming
/// `<hostname>-<pid>`. It breaks a lock left by a dead process on the same
/// host by itself, but one naming another host it treats as held, maybe over
/// a network filesystem, and every `cefsrc` then fails to start with a bare
/// "Failed to start". A native cache directory outlives the process, and a
/// laptop's hostname changes with the network it is on, so a hard kill is
/// enough to lock HTML sources out for good.
///
/// The cache root is this instance's own (one per data directory), so a lock
/// from another host is stale. A lock from this host is left to Chromium,
/// which can tell whether that process still runs. Must run before CEF
/// initializes, i.e. before the first `cefsrc` is made.
#[cfg(unix)]
pub fn clear_stale_singleton_lock(root: &Path) {
    let lock = root.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return;
    };
    let Ok(host) = hostname::get() else {
        return;
    };
    if lock_host(&target.to_string_lossy()) == Some(host.to_string_lossy().as_ref()) {
        return;
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let path = root.join(name);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Could not remove stale {}: {}", path.display(), e);
                return;
            }
        }
    }
    warn!(
        "Removed a CEF cache lock left by {} - a Strom that was killed on another \
         hostname. Without this every HTML source would fail to start",
        target.display()
    );
}

/// The host a `SingletonLock` target names: everything before the last `-`.
/// Hostnames may contain `-`; the pid cannot.
#[cfg(unix)]
fn lock_host(target: &str) -> Option<&str> {
    let (host, pid) = target.rsplit_once('-')?;
    pid.parse::<u32>().ok().map(|_| host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use strom_types::{BlockInstance, Element, PropertyValue};

    fn flow_with(blocks: Vec<BlockInstance>, elements: Vec<Element>) -> Flow {
        let mut flow = Flow::new("f");
        flow.blocks = blocks;
        flow.elements = elements;
        flow
    }

    fn html_block(id: &str, profile: Option<&str>) -> BlockInstance {
        let mut properties = HashMap::new();
        if let Some(name) = profile {
            properties.insert(
                html_input::BROWSER_PROFILE_PROPERTY.to_string(),
                PropertyValue::String(name.to_string()),
            );
        }
        BlockInstance {
            id: id.to_string(),
            block_definition_id: html_input::BLOCK_ID.to_string(),
            name: None,
            properties,
            position: strom_types::block::Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }
    }

    fn cefsrc(id: &str) -> Element {
        Element {
            id: id.to_string(),
            element_type: "cefsrc".to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        }
    }

    fn dirs(root: &Path) -> HashSet<String> {
        std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn startup_keeps_the_profiles_flows_use_and_removes_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let kept = flow_with(
            vec![html_block("b1", None), html_block("b2", Some("shared"))],
            vec![cefsrc("e1")],
        );
        let gone = flow_with(vec![html_block("b1", None)], vec![]);
        let all = profiles_in_use(root, [&kept, &gone]);
        for dir in &all {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir_all(root.join("strom-named-orphan")).unwrap();
        // Chromium's own and the fake media devices are not ours to remove.
        std::fs::create_dir_all(root.join("Default")).unwrap();
        std::fs::create_dir_all(root.join("strom-fake-media")).unwrap();

        remove_unused_profiles(root, [&kept]);

        let expected: HashSet<String> = profiles_in_use(root, [&kept])
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .chain(["Default".to_string(), "strom-fake-media".to_string()])
            .collect();
        assert_eq!(dirs(root), expected);
        assert_eq!(
            expected.len(),
            5,
            "b1, the shared profile, e1 and the two others"
        );
    }

    #[test]
    fn a_deleted_flow_takes_its_own_profiles_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let deleted = flow_with(
            vec![html_block("b1", None), html_block("b2", Some("shared"))],
            vec![cefsrc("e1")],
        );
        let other = flow_with(vec![html_block("b1", None)], vec![cefsrc("e1")]);
        for dir in profiles_in_use(root, [&deleted, &other]) {
            std::fs::create_dir_all(dir).unwrap();
        }

        remove_flow_profiles(root, &deleted.id);

        let mut expected: HashSet<String> = profiles_in_use(root, [&other])
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        expected.insert("strom-named-shared".to_string());
        assert_eq!(dirs(root), expected);
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_from_another_host_is_cleared_and_one_from_this_host_is_not() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let lock = root.join("SingletonLock");

        std::os::unix::fs::symlink("old-laptop.example.com-4242", &lock).unwrap();
        std::os::unix::fs::symlink("/tmp/gone/SingletonSocket", root.join("SingletonSocket"))
            .unwrap();
        clear_stale_singleton_lock(root);
        assert!(std::fs::symlink_metadata(&lock).is_err());
        assert!(std::fs::symlink_metadata(root.join("SingletonSocket")).is_err());

        let here = hostname::get().unwrap().to_string_lossy().to_string();
        std::os::unix::fs::symlink(format!("{}-4242", here), &lock).unwrap();
        clear_stale_singleton_lock(root);
        assert!(std::fs::symlink_metadata(&lock).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_host_is_everything_before_the_pid() {
        assert_eq!(lock_host("my-host-name-123"), Some("my-host-name"));
        assert_eq!(lock_host("no-pid-here"), None);
    }
}
