use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique, empty scratch directory for a test registry.
fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "processkit-cli-registry-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn shared_setup_read_error_pins_the_exit_code_and_diagnostic() {
    let err = setup_read_error(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
    assert_eq!(err.code(), exit::SETUP);
    assert_eq!(err.to_string(), "could not read the run registry: denied");
}

#[test]
fn point_probe_reads_only_the_requested_validated_record() {
    let dir = scratch("point-probe");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("target", Some("endpoint-a"), SystemTime::now())
        .expect("register target");
    fs::write(dir.join("unrelated.json"), "not valid JSON")
        .expect("write unrelated corrupt record");

    let entry = registry
        .probe_entry(registration.record_path())
        .expect("probe exact target")
        .expect("target exists");
    assert_eq!(entry.record.run_id, "target");
    assert_eq!(entry.health, Health::Live);

    assert!(
        registry
            .probe_entry(&dir.join("missing.json"))
            .expect("probe a missing target")
            .is_none(),
        "a missing exact record is distinct from corrupt content"
    );
    assert_eq!(
        registry
            .probe_entry(&dir.join("unrelated.json"))
            .expect_err("corrupt target content is a hard read failure")
            .kind(),
        io::ErrorKind::InvalidData
    );

    drop(registration);
    let _ = fs::remove_dir_all(&dir);
}

/// Test-only: set `path`'s mtime `age` in the past, without a real sleep — used
/// to age an orphan-lock fixture past [`ORPHAN_LOCK_MIN_AGE`] so `prune`'s second
/// pass actually considers it a candidate (see [R-01]). Works for both a regular
/// file and a directory (the [K-014] fixture the probe-error orphan test below
/// uses is a directory), which is why unix opens it plainly (permission to change
/// an owned file/directory's timestamps does not depend on the fd's access mode)
/// while Windows must explicitly ask for `FILE_FLAG_BACKUP_SEMANTICS` to get a
/// handle on a directory at all, plus write access for `SetFileTime`.
#[cfg(unix)]
fn backdate(path: &Path, age: Duration) {
    let file = File::open(path).expect("open the fixture to backdate its mtime");
    file.set_modified(SystemTime::now() - age)
        .expect("backdate the fixture's mtime");
}

#[cfg(windows)]
fn backdate(path: &Path, age: Duration) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("open the fixture to backdate its mtime");
    file.set_modified(SystemTime::now() - age)
        .expect("backdate the fixture's mtime");
}

/// The registry directory is created restricted to its owner (`0700` / an
/// owner-only protected DACL) — a control channel address must not be world
/// readable.
#[test]
fn directory_is_created_owner_only() {
    let dir = scratch("perms");
    let _registry = Registry::open_in(dir.clone()).expect("open registry");
    assert!(
        platform::is_owner_only(&dir).expect("read permissions"),
        "the registry directory must be restricted to its owner"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `open_read_only` is `list`'s entry point and must never create registry
/// state: scanning an empty registry (one whose directory does not exist yet)
/// must leave the directory absent, not conjure it into existence just to
/// discover there is nothing in it.
#[test]
fn open_read_only_does_not_create_the_directory() {
    let dir = scratch("read-only-absent");
    assert!(!dir.exists(), "the scratch fixture starts absent");

    let registry = Registry::open_read_only_in(dir.clone());
    assert!(
        !dir.exists(),
        "a read-only open must not create the registry directory"
    );
    assert!(
        registry.entries().expect("scan").is_empty(),
        "a missing directory reads back as an empty registry"
    );
    assert!(
        !dir.exists(),
        "scanning a missing directory must not create it either"
    );
}

/// `open_read_only` must not re-assert (or otherwise touch) the permissions of
/// an *existing* registry directory — only the mutating [`Registry::open`] /
/// [`Registry::open_in`] path is allowed to do that. Unix-only: it is the
/// platform whose owner-only enforcement (`chmod`) is cheap to defeat and
/// re-check from a plain `std::fs` test without extra Windows ACL plumbing.
#[cfg(unix)]
#[test]
fn open_read_only_does_not_touch_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("read-only-existing-perms");
    let _mutating = Registry::open_in(dir.clone()).expect("create the registry once");
    assert!(platform::is_owner_only(&dir).expect("read permissions"));

    // Loosen the directory's permissions out-of-band, simulating an operator (or
    // a prior process) having widened them for some unrelated reason.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("loosen permissions");

    let read_only = Registry::open_read_only_in(dir.clone());
    assert!(
        read_only.entries().expect("scan").is_empty(),
        "an empty existing directory still reads back empty"
    );
    assert!(
        !platform::is_owner_only(&dir).expect("read permissions"),
        "a read-only open must leave a pre-existing directory's permissions alone"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A registered run writes a well-formed record: the run id, the endpoint it was
/// given (here `None`), the start timestamp, and the advisory-lock liveness
/// signal — and carries no PID.
#[test]
fn register_writes_a_record_without_a_pid() {
    let dir = scratch("record");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let started = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    let registration = registry
        .register_plain("run-42", None, started)
        .expect("register run");

    let text = fs::read_to_string(registration.record_path()).expect("read record");
    let record: Record = serde_json::from_str(&text).expect("parse record");
    assert_eq!(record.run_id, "run-42");
    assert_eq!(record.registry_version, REGISTRY_VERSION);
    assert!(
        record.endpoint.is_none(),
        "register stores the endpoint it is given verbatim — here None"
    );
    assert_eq!(record.started_at, events::format_rfc3339_utc(started));
    assert_eq!(record.liveness.kind, LIVENESS_ADVISORY_LOCK);
    assert!(record.liveness.lock_file.ends_with(".lock"));
    assert!(
        !text.contains("\"pid\""),
        "a record must not be keyed by PID: {text}"
    );

    registration.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// T-215's producer side: a registered run publishes the two redaction-safe
/// command fields — the fingerprint the JSONL stream carries for the same run,
/// and the worker-shape hint — and publishes **nothing else** about the command.
/// The argv here is a recognized MSBuild worker shape carrying a secret-looking
/// token, so the test pins both at once: the classified hint is written, and no
/// fragment of the command line reaches the on-disk record.
#[test]
fn register_publishes_a_fingerprint_and_hint_but_never_argv() {
    let dir = scratch("record-command");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let argv = [
        "C:\\dotnet\\MSBuild.dll",
        "/nodemode:1",
        "/nodeReuse:true",
        "/p:ApiKey=hunter2-do-not-log",
    ];
    let fingerprint = events::CommandFingerprint::for_argv(argv);
    let registration = registry
        .register("run-cmd", None, SystemTime::now(), &fingerprint)
        .expect("register run");

    let text = fs::read_to_string(registration.record_path()).expect("read record");
    let record: Record = serde_json::from_str(&text).expect("parse record");
    assert_eq!(
        record.argv_sha256.as_deref(),
        Some(fingerprint.argv_sha256.as_str()),
        "the record carries the same fingerprint the run's events carry"
    );
    assert_eq!(
        record.hint.as_deref(),
        Some("msbuild_node_reuse"),
        "a recognized worker shape is published as its catalog label"
    );
    for fragment in ["hunter2", "ApiKey", "MSBuild.dll", "nodeReuse"] {
        assert!(
            !text.contains(fragment),
            "no argv content may reach a registry record ({fragment:?}): {text}"
        );
    }

    registration.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// The same fields, for the common case: an argv matching no catalog rule still
/// gets a fingerprint (it is derived from argv, so it always exists) but no hint
/// — `null`, not an invented label.
#[test]
fn register_publishes_no_hint_for_an_unrecognized_command() {
    let dir = scratch("record-command-unclassified");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register(
            "run-plain",
            None,
            SystemTime::now(),
            &events::CommandFingerprint::for_argv(["cmd", "/c", "echo hi"]),
        )
        .expect("register run");

    let record: Record =
        serde_json::from_str(&fs::read_to_string(registration.record_path()).expect("read"))
            .expect("parse record");
    assert!(
        record
            .argv_sha256
            .as_deref()
            .is_some_and(is_valid_argv_sha256),
        "every run publishes a well-formed fingerprint: {:?}",
        record.argv_sha256
    );
    assert!(
        record.hint.is_none(),
        "an unrecognized shape publishes no hint: {:?}",
        record.hint
    );

    registration.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// Backward compatibility, the read side of T-215's additive change: a record
/// written **before** these fields existed — no `argv_sha256`, no `hint` key at
/// all — still parses, with both fields simply absent. It is scanned, probed, and
/// listed exactly as it always was; nothing about the entry depends on the new
/// fields being there.
#[test]
fn a_record_without_the_command_fields_still_reads() {
    let dir = scratch("record-legacy");
    fs::create_dir_all(&dir).expect("create the registry directory");
    // Byte-for-byte the record shape a pre-T-215 runner wrote.
    let legacy = "{\"registry_version\":1,\"run_id\":\"legacy\",\"endpoint\":null,\
         \"started_at\":\"2026-07-22T00:00:00.000Z\",\
         \"liveness\":{\"kind\":\"advisory_lock\",\"lock_file\":\"legacy.lock\"}}";
    let record = parse_and_validate_record(legacy).expect("a pre-T-215 record still parses");
    assert_eq!(record.run_id, "legacy");
    assert!(
        record.argv_sha256.is_none() && record.hint.is_none(),
        "absent fields read back as absent, never as an error or a fabricated value"
    );

    // …and the whole scan path agrees: the entry is found and probed as usual.
    fs::write(dir.join("legacy.json"), legacy).expect("write the legacy record");
    fs::write(dir.join("legacy.lock"), b"").expect("write an unlocked lock file");
    let entries = Registry::open_read_only_in(dir.clone())
        .entries()
        .expect("scan");
    assert_eq!(
        entries.len(),
        1,
        "a legacy record is a perfectly good entry"
    );
    assert_eq!(entries[0].record.run_id, "legacy");
    assert_eq!(
        entries[0].health,
        Health::Stale,
        "its liveness is decided by its lock, exactly as before"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The other direction of the same compatibility claim (the one that makes
/// [`REGISTRY_VERSION`] not need a bump): a record written by a **newer** writer,
/// carrying a field this binary has never heard of, is read as an ordinary
/// record — the unknown field is ignored, not treated as corruption. Both
/// directions matter in the mixed registry a mid-upgrade user actually has.
#[test]
fn a_record_with_an_unknown_field_still_reads() {
    let from_the_future = "{\"registry_version\":1,\"run_id\":\"future\",\"endpoint\":null,\
         \"started_at\":\"2026-07-22T00:00:00.000Z\",\
         \"argv_sha256\":null,\"hint\":null,\"some_future_field\":{\"a\":1},\
         \"liveness\":{\"kind\":\"advisory_lock\",\"lock_file\":\"future.lock\"}}";
    let record =
        parse_and_validate_record(from_the_future).expect("an unknown field is not corruption");
    assert_eq!(record.run_id, "future");
}

/// The Drop-backstop this task adds: a [`ReservedEntry`] that is dropped before
/// its record is ever published (here simulated directly, the same shape
/// `Registry::register` hits when its `fs::write` of the JSON record fails and
/// returns early with `?`, before it ever calls `disarm`) must delete its
/// freshly created `.lock` file — never leave it as an orphan invisible to
/// `scan()` (which only walks `.json` files).
#[test]
fn reserved_entry_drop_backstop_removes_the_lock_file_when_never_published() {
    let dir = scratch("reserve-drop-backstop");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let reserved = registry.reserve_entry().expect("reserve an entry");
    let lock_path = reserved.lock_path.clone();
    assert!(
        lock_path.exists(),
        "reserve_entry creates the lock file up front"
    );

    // Never publish the record (no `fs::write` of the `.json`, no `disarm`) —
    // just drop the reservation, exactly as an early `?` return in `register`
    // would.
    drop(reserved);

    assert!(
        !lock_path.exists(),
        "dropping an unpublished reservation must remove its lock file"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// T-230 regression at the earlier boundary: cleanup is useful before a
/// [`ReservedEntry`] exists at all. `reserve_entry` now constructs the guard
/// immediately after `create_new`, so an error or retry in either lock probe
/// drops this exact shape and removes the path best-effort.
#[test]
fn early_reservation_cleanup_removes_the_new_lock_path() {
    let dir = scratch("reserve-early-cleanup");
    fs::create_dir_all(&dir).expect("create scratch registry directory");
    let lock_path = dir.join("early.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .expect("create the lock file");
    let created = CreatedLock::new(lock, lock_path.clone());

    drop(created);

    assert!(
        !lock_path.exists(),
        "an armed guard removes the path even before ReservedEntry construction"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A clean exit removes the entry: files gone, and the scan sees nothing.
#[test]
fn clean_removal_deletes_the_entry() {
    let dir = scratch("remove");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("run-clean", None, SystemTime::now())
        .expect("register run");
    let record_path = registration.record_path().to_owned();
    let lock_path = registration.lock_path().to_owned();

    assert_eq!(registry.entries().expect("scan").len(), 1);
    assert!(record_path.exists() && lock_path.exists());

    registration.remove();
    assert!(
        !record_path.exists() && !lock_path.exists(),
        "a clean exit must delete both entry files"
    );
    assert!(
        registry.entries().expect("scan").is_empty(),
        "a removed entry must not be listed"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The heart of the task: an abruptly-killed runner leaves its record *and* lock
/// file on disk, yet the entry is detectably stale — because liveness is the
/// released lock, not the file's existence.
#[test]
fn stale_entry_is_detected_without_relying_on_file_existence() {
    let dir = scratch("stale");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("run-victim", None, SystemTime::now())
        .expect("register run");
    let record_path = registration.record_path().to_owned();
    let lock_path = registration.lock_path().to_owned();

    // While the runner is alive it holds the lock: the entry reads as live.
    let live = registry.entries().expect("scan");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].health, Health::Live);

    // Simulate an abrupt kill: release the lock but leave the files behind.
    registration.simulate_abrupt_death();

    // The files still exist — so file existence cannot be what marks staleness…
    assert!(
        record_path.exists() && lock_path.exists(),
        "the abrupt-death fixture must leave both files on disk"
    );
    // …yet the released lock makes the entry detectably stale.
    let stale = registry.entries().expect("scan");
    assert_eq!(stale.len(), 1);
    assert_eq!(
        stale[0].health,
        Health::Stale,
        "an entry whose runner died must read as stale"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Concurrent runs get independent entries: distinct files, both live, and
/// removing one leaves the other untouched.
#[test]
fn concurrent_runs_get_independent_entries() {
    let dir = scratch("concurrent");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let now = SystemTime::now();
    let first = registry
        .register_plain("run-a", None, now)
        .expect("register a");
    let second = registry
        .register_plain("run-b", None, now)
        .expect("register b");
    assert_ne!(
        first.record_path(),
        second.record_path(),
        "each run gets its own file"
    );

    let both = registry.entries().expect("scan");
    assert_eq!(both.len(), 2);
    assert!(both.iter().all(|entry| entry.health == Health::Live));

    first.remove();
    let remaining = registry.entries().expect("scan");
    assert_eq!(remaining.len(), 1, "removing one leaves the other");
    assert_eq!(remaining[0].record.run_id, "run-b");
    assert_eq!(
        remaining[0].health,
        Health::Live,
        "the surviving run stays live"
    );

    second.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// Write a hand-crafted registry record (`<stem>.json`) with a chosen `lock_file`
/// value, simulating a corrupt or adversarial deserialized entry a real runner
/// would never write (`register` only ever mints a safe `run-<hex>-<hex>.lock`).
fn write_record(dir: &Path, stem: &str, run_id: &str, lock_file: &str) {
    write_record_with_endpoint(dir, stem, run_id, lock_file, None);
}

/// Like [`write_record`], but also publishing an `endpoint` — the control-transport
/// address a record carries, and the value T-207's socket reap validates by shape
/// before it deletes anything through it.
fn write_record_with_endpoint(
    dir: &Path,
    stem: &str,
    run_id: &str,
    lock_file: &str,
    endpoint: Option<&str>,
) {
    let record = Record {
        registry_version: REGISTRY_VERSION,
        run_id: run_id.to_string(),
        endpoint: endpoint.map(str::to_string),
        started_at: events::format_rfc3339_utc(SystemTime::now()),
        // These fixtures exist to exercise the `lock_file`/`endpoint` guards;
        // publishing no command metadata keeps them focused (and keeps them
        // covering the "record without it" shape every consumer must handle).
        argv_sha256: None,
        hint: None,
        liveness: Liveness {
            kind: LIVENESS_ADVISORY_LOCK.to_string(),
            lock_file: lock_file.to_string(),
        },
    };
    let json = serde_json::to_string(&record).expect("serialize the record");
    fs::write(dir.join(format!("{stem}.json")), json).expect("write the record");
}

/// A unique, not-yet-created path of exactly the shape `ControlServer::bind`
/// creates a private control-socket directory at — `pkc-<token>` directly inside
/// the platform temp directory, which is always one of `control::socket_base_dirs`'
/// bases. The token is per-call-site plus a process-wide counter on top of the
/// pid, for the same reason [`scratch`] carries one (see [K-026]).
///
/// Deliberately much shorter than a [`scratch`] name: a real unix socket is bound
/// inside it, and the whole path has to stay within `sockaddr_un::sun_path` on the
/// shortest platform — macOS, whose temp directory is itself ~50 characters (see
/// [K-009]). Keep the tags short for the same reason.
#[cfg(unix)]
fn socket_dir_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{}t{tag}-{}-{n}",
        crate::control::SOCKET_DIR_PREFIX,
        std::process::id()
    ))
}

/// The counterpart to [`socket_dir_path`] for a directory that must **not** be a
/// reap candidate: a unique, short, not-yet-created directory in the platform temp
/// directory whose name is not the published `pkc-` form, used as the parent of an
/// off-base fixture (or as a symlink's target). Short for the same `sun_path`
/// reason [`socket_dir_path`] is.
#[cfg(unix)]
fn off_base_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pkt{tag}-{}-{n}", std::process::id()))
}

/// A ready-made leftover of an abruptly-killed runner's control transport: the
/// private `pkc-…` directory of [`socket_dir_path`], with a **real** bound unix
/// socket inside it. Returns the directory and the endpoint string a record would
/// publish for it.
#[cfg(unix)]
fn socket_fixture(tag: &str) -> (PathBuf, String) {
    let dir = socket_dir_path(tag);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("create the private control-socket directory");
    let endpoint = bind_socket_in(&dir);
    (dir, endpoint)
}

/// Bind a real unix socket at `<dir>/c.sock` and return its path as the endpoint a
/// record publishes. The listener is dropped immediately on purpose: a bound unix
/// socket file outlives its listener (only an unlink removes it), which is exactly
/// the leftover an abruptly-killed runner strands on disk.
#[cfg(unix)]
fn bind_socket_in(dir: &Path) -> String {
    let path = dir.join(crate::control::SOCKET_FILE_NAME);
    let listener =
        std::os::unix::net::UnixListener::bind(&path).expect("bind the fixture's control socket");
    drop(listener);
    path.to_str()
        .expect("the fixture's socket path is UTF-8")
        .to_string()
}

/// Like [`write_record`], but with an explicit `started_at` string instead of
/// the current time — for exercising [`is_valid_rfc3339_millis_utc`]'s
/// corrupt-record guard with values a real runner would never write.
fn write_record_with_started_at(dir: &Path, stem: &str, run_id: &str, started_at: &str) {
    let record = Record {
        registry_version: REGISTRY_VERSION,
        run_id: run_id.to_string(),
        endpoint: None,
        started_at: started_at.to_string(),
        argv_sha256: None,
        hint: None,
        liveness: Liveness {
            kind: LIVENESS_ADVISORY_LOCK.to_string(),
            lock_file: format!("{stem}.lock"),
        },
    };
    let json = serde_json::to_string(&record).expect("serialize the record");
    fs::write(dir.join(format!("{stem}.json")), json).expect("write the record");
}

/// A platform-absolute path (never a simple in-directory name).
fn absolute_escape() -> &'static str {
    if cfg!(windows) {
        "C:\\Windows\\Temp\\escape.lock"
    } else {
        "/tmp/escape.lock"
    }
}

/// A registry record's raw JSON with `argv_sha256`/`hint` set to arbitrary
/// (here deliberately malformed) values — the corrupt or hand-edited shape no
/// runner writes, for exercising the read-side guards on those two fields.
fn record_json_with_command_fields(run_id: &str, argv_sha256: &str, hint: &str) -> String {
    // Built through `serde_json` rather than string-formatted so a value
    // carrying quotes/newlines/control characters is escaped into *valid* JSON:
    // the point of these fixtures is a well-formed file with a bad field value,
    // not a broken file the JSON parser would reject before any guard ran.
    serde_json::json!({
        "registry_version": REGISTRY_VERSION,
        "run_id": run_id,
        "endpoint": serde_json::Value::Null,
        "started_at": "2026-07-22T00:00:00.000Z",
        "argv_sha256": argv_sha256,
        "hint": hint,
        "liveness": { "kind": LIVENESS_ADVISORY_LOCK, "lock_file": format!("{run_id}.lock") },
    })
    .to_string()
}

/// The read-side contract for the two new fields: a value that is not the exact
/// shape a runner writes is **dropped**, and the record itself survives. Every
/// other field keeps its value, so the entry stays fully usable — the field is
/// simply "not reported", the same state a record written before these fields
/// existed is in.
#[test]
fn a_malformed_command_field_is_dropped_not_the_record() {
    let record = parse_and_validate_record(&record_json_with_command_fields(
        "victim",
        // Uppercase hex: a digest no writer of this format produces.
        "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        // A label carrying a newline — what would otherwise forge an extra row
        // in `list`'s table — plus an ANSI escape.
        "msbuild\n\u{1b}[31mFAKE-ROW",
    ))
    .expect("a malformed command field must not discard the record");
    assert_eq!(record.run_id, "victim", "every other field is untouched");
    assert_eq!(record.started_at, "2026-07-22T00:00:00.000Z");
    assert!(
        record.argv_sha256.is_none(),
        "a malformed fingerprint is dropped: {:?}",
        record.argv_sha256
    );
    assert!(
        record.hint.is_none(),
        "a malformed hint is dropped: {:?}",
        record.hint
    );
}

/// Why that is the right verdict, demonstrated where it actually bites: a
/// **live** run whose record has a corrupt `hint` (a hand-edited byte, a partial
/// write) stays visible to the scan every client shares. Discarding the record
/// over a field nothing acts on would hide a running run from `list`, and with it
/// from `wait` and from the `inspect`/`cancel`/`kill` resolution that matches on
/// `run_id` — a cosmetic field silently disarming the control plane.
#[test]
fn a_live_entry_with_a_corrupt_hint_is_still_found() {
    let dir = scratch("corrupt-hint-live");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("still-running", None, SystemTime::now())
        .expect("register run");

    // Corrupt only the `hint` value of the published record, leaving the held
    // liveness lock — and every other field, including the `lock_file` name that
    // points at it — exactly as they were.
    let corrupted = record_json_with_command_fields(
        "still-running",
        &crate::hash::sha256_hex(b"whatever"),
        "not a valid label!",
    )
    .replace("still-running.lock", &file_name(registration.lock_path()));
    fs::write(registration.record_path(), corrupted)
        .expect("rewrite the record with a corrupt hint");

    let entries = registry.entries().expect("scan");
    assert_eq!(
        entries.len(),
        1,
        "a corrupt cosmetic field must not hide a live run from the scan"
    );
    assert_eq!(entries[0].record.run_id, "still-running");
    assert_eq!(
        entries[0].health,
        Health::Live,
        "the run is still live, and still reported as such"
    );
    assert!(
        entries[0].record.hint.is_none(),
        "the unusable value itself is dropped: {:?}",
        entries[0].record.hint
    );

    registration.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// [`is_valid_argv_sha256`]'s boundary table. A hand-rolled validator is exactly
/// the kind that passes by inspection and fails on an edge case ([K-030]), so
/// every boundary is spelled out: the accepted length either side, the case of
/// the hex digits, a non-hex letter, and a multi-byte character that makes the
/// byte length "right" while the character length is not.
#[test]
fn argv_sha256_guard_accepts_only_a_full_lowercase_hex_digest() {
    let real = crate::hash::sha256_hex(b"processkit-cli");
    assert!(
        is_valid_argv_sha256(&real),
        "the digest this project actually produces must pass: {real}"
    );
    assert!(is_valid_argv_sha256(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    ));

    for rejected in [
        // Empty, and the two lengths either side of 64.
        "",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
        // Uppercase hex — a spelling no writer of this format emits, and a
        // second spelling of one fingerprint if accepted.
        "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF",
        // A non-hex letter, in the first and in the last position.
        "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
        // Surrounding or embedded whitespace, and a control character.
        " 123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd\ne",
        // 64 *bytes* but 63 characters: the length check alone would pass it,
        // the per-byte hex check is what refuses it.
        "α123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        assert!(
            !is_valid_argv_sha256(rejected),
            "not a lowercase-hex SHA-256 digest: {rejected:?}"
        );
    }
}

/// [`is_valid_hint`]'s boundary table, in the same [K-030] spirit: the label
/// shape `docs/schema.md` requires is accepted at both length boundaries, and
/// everything that would make the value more than a category name — a
/// separator, whitespace, a newline forging a table row, an ANSI escape, a NUL,
/// a non-ASCII character, or an unbounded blob — is refused.
#[test]
fn hint_guard_accepts_label_shapes_and_refuses_everything_else() {
    for accepted in [
        "msbuild_node_reuse",
        "a",
        "gradle_daemon_7",
        "_leading_underscore",
        &"x".repeat(MAX_HINT_LEN),
    ] {
        assert!(
            is_valid_hint(accepted),
            "a plain snake_case label must be accepted: {accepted:?}"
        );
    }

    for rejected in [
        "",
        &"x".repeat(MAX_HINT_LEN + 1),
        "MSBuild_Node_Reuse",
        "msbuild node reuse",
        "msbuild-node-reuse",
        "msbuild.node.reuse",
        "msbuild/node",
        "msbuild\nnode",
        "msbuild\u{1b}[31m",
        "msbuild\0node",
        "msbuildα",
    ] {
        assert!(
            !is_valid_hint(rejected),
            "not a category label: {rejected:?}"
        );
    }
}

/// Anti-drift, in both directions of the one contract that spans two modules:
/// every label the **real** classifier catalog can emit passes the record guard
/// that reads it back. A new `HINT_RULES` entry spelled in a shape this guard
/// refuses would otherwise publish a label that silently vanished at scan time —
/// visible nowhere except as a mysteriously empty column. Asserted against the
/// catalog itself ([`events::hint_labels`]), never a copy of it.
#[test]
fn hint_labels_from_the_real_catalog_pass_the_record_guard() {
    let mut labels = 0usize;
    for label in events::hint_labels() {
        assert!(
            is_valid_hint(label),
            "the classifier can emit {label:?}, so the record guard must accept it"
        );
        labels += 1;
    }
    assert!(
        labels > 0,
        "the catalog is not empty, so this asserted something"
    );
}

/// The names a live runner actually mints, plus benign edge cases that merely
/// *resemble* a reserved device, are all accepted — the guard must not discard a
/// legitimate entry (the positive case).
#[test]
fn simple_lock_file_names_are_accepted() {
    for name in [
        "run-00000000000000000000000000000000-0000000000000000.lock",
        "run-0123456789abcdef.lock",
        "a.lock",
        // Resembles a device name but is not one: extra letters / an out-of-range
        // ordinal / no ordinal at all.
        "console.lock",
        "nula.lock",
        "com10.lock",
        "com0.lock",
        "lpt.lock",
    ] {
        assert!(
            is_simple_lock_file_name(name),
            "a plain single-component .lock name must be accepted: {name:?}"
        );
    }
}

/// Every way a `lock_file` value can fail the simple-name contract — path
/// traversal, absolute paths, embedded separators, a missing/wrong extension,
/// NUL/control characters, the `:` drive/stream delimiter, and Windows reserved
/// device names (bare and in their name-plus-extension aliasing form, including
/// the superscript `COM`/`LPT` variants) — is rejected.
#[test]
fn unsafe_lock_file_names_are_rejected() {
    for name in [
        // Empty / traversal / absolute.
        "",
        "..",
        ".",
        "../escape.lock",
        "..\\escape.lock",
        "/tmp/escape.lock",
        "/etc/passwd.lock",
        "C:\\Windows\\escape.lock",
        "C:escape.lock",
        // Embedded path separators / drive-or-stream delimiter.
        "sub/dir.lock",
        "sub\\dir.lock",
        "stream:evil.lock",
        // Missing or wrong extension.
        "run-0000",
        "run-0000.txt",
        "run-0000.lock.bak",
        ".lock",
        // NUL / control characters.
        "run-0000\0.lock",
        "run-0000\t.lock",
        "run-0000\n.lock",
        // Windows reserved device names, bare and with an added extension chain.
        "CON.lock",
        "con.lock",
        "PRN.lock",
        "AUX.lock",
        "NUL.lock",
        "NUL.tar.gz.lock",
        "COM1.lock",
        "com9.lock",
        "LPT1.lock",
        "lpt9.lock",
        // Latin-1 superscript device-name aliases (still reserved).
        "COM\u{b9}.lock",
        "COM\u{b2}.lock",
        "COM\u{b3}.lock",
        "LPT\u{b9}.lock",
        "LPT\u{b2}.lock",
        "LPT\u{b3}.lock",
    ] {
        assert!(
            !is_simple_lock_file_name(name),
            "an unsafe lock_file value must be rejected: {name:?}"
        );
    }
}

/// A record whose `lock_file` is not a simple in-directory name — a `..`
/// traversal, an absolute path, or a Windows reserved device name — is a corrupt
/// entry: the scan skips it (never joining the value onto the registry directory
/// to probe a file outside it) while a well-formed sibling entry is still scanned
/// and returned. Proves the guard both defends the directory boundary and does not
/// abort the whole scan over one bad record.
#[test]
fn entries_skip_unsafe_lock_files_without_aborting_the_scan() {
    let dir = scratch("unsafe-lock");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    write_record(&dir, "escaper-rel", "escaper-rel", "../escape.lock");
    write_record(&dir, "escaper-abs", "escaper-abs", absolute_escape());
    write_record(&dir, "device", "device", "NUL.tar.gz.lock");

    // A well-formed live entry alongside the corrupt ones.
    let good = registry
        .register_plain("good", None, SystemTime::now())
        .expect("register the good run");

    let entries = registry.entries().expect("scan");
    assert_eq!(
        entries.len(),
        1,
        "every unsafe entry is skipped and only the well-formed one survives"
    );
    assert_eq!(entries[0].record.run_id, "good");
    assert_eq!(entries[0].health, Health::Live);

    good.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// `is_valid_rfc3339_millis_utc` accepts every value the formatter it mirrors can
/// actually produce (the positive case a corrupt-record guard must not
/// accidentally reject) and rejects the shapes a hand-edited or truncated record
/// could plausibly carry instead.
#[test]
fn started_at_validator_accepts_the_formatter_output_and_rejects_malformed_values() {
    for secs in [0u64, 1, 59, 3599, 86_399, 1_700_000_000] {
        for millis in [0u64, 5, 500, 999] {
            let formatted = events::format_rfc3339_utc(
                UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis),
            );
            assert!(
                is_valid_rfc3339_millis_utc(&formatted),
                "the formatter's own output must validate: {formatted:?}"
            );
        }
    }

    for bad in [
        "",
        "not-a-timestamp",
        "2026-07-22T00:00:00Z",       // missing millisecond field
        "2026-07-22 00:00:00.000Z",   // space instead of `T`
        "2026-07-22T00:00:00.000",    // missing trailing `Z`
        "2026-13-01T00:00:00.000Z",   // month out of range
        "2026-07-32T00:00:00.000Z",   // day out of range
        "2026-07-22T24:00:00.000Z",   // hour out of range
        "2026-07-22T00:60:00.000Z",   // minute out of range
        "2026-07-22T00:00:60.000Z",   // second out of range
        "2026-07-22T00:00:00.000Z\0", // trailing NUL
        "20260722T000000.000Z",       // no separators at all
        "2026-02-31T00:00:00.000Z",   // February never has 31 days
        "2026-02-30T00:00:00.000Z",   // February never has 30 days
        "2026-02-29T00:00:00.000Z",   // 2026 is not a leap year
        "2100-02-29T00:00:00.000Z",   // century not divisible by 400: not a leap year
        "2026-04-31T00:00:00.000Z",   // April is a 30-day month
        "2026-06-31T00:00:00.000Z",   // June is a 30-day month
        "2026-09-31T00:00:00.000Z",   // September is a 30-day month
        "2026-11-31T00:00:00.000Z",   // November is a 30-day month
    ] {
        assert!(
            !is_valid_rfc3339_millis_utc(bad),
            "a malformed started_at value must be rejected: {bad:?}"
        );
    }

    // Calendar-valid edge cases that must still be accepted: leap-year February 29
    // (both the ordinary `% 4 == 0` rule and the `% 400 == 0` century exception),
    // and the last day of every 30/31-day month.
    for good in [
        "2024-02-29T00:00:00.000Z", // ordinary leap year (divisible by 4, not by 100)
        "2000-02-29T00:00:00.000Z", // century leap year (divisible by 400)
        "2026-02-28T00:00:00.000Z", // last day of February in a non-leap year
        "2026-04-30T00:00:00.000Z", // last day of a 30-day month
        "2026-01-31T00:00:00.000Z", // last day of a 31-day month
        "2026-12-31T00:00:00.000Z", // last day of the year
    ] {
        assert!(
            is_valid_rfc3339_millis_utc(good),
            "a calendar-valid started_at value must be accepted: {good:?}"
        );
    }
}

/// A record whose `started_at` is malformed (not the runner's own
/// [`events::format_rfc3339_utc`] shape) is corrupt-record noise: the scan skips
/// it — never listing or sorting a fabricated timestamp as if it were real —
/// while a well-formed sibling entry is still scanned and returned. Mirrors
/// `entries_skip_unsafe_lock_files_without_aborting_the_scan`'s degradation
/// proof for the `started_at` field.
#[test]
fn entries_skip_malformed_started_at_without_aborting_the_scan() {
    let dir = scratch("bad-started-at");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    write_record_with_started_at(&dir, "garbage", "garbage", "not-a-timestamp");
    write_record_with_started_at(&dir, "truncated", "truncated", "2026-07-22T00:00:00Z");

    let good = registry
        .register_plain("good", None, SystemTime::now())
        .expect("register the good run");

    let entries = registry.entries().expect("scan");
    assert_eq!(
        entries.len(),
        1,
        "every malformed-started_at entry is skipped and only the well-formed one survives"
    );
    assert_eq!(entries[0].record.run_id, "good");

    good.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// Unix: a lock file that is a *symlink* is refused at open time (`O_NOFOLLOW`),
/// even though its name passes the simple-name check — so a record pointing a
/// valid-looking lock name at a symlink still shows up in the scan (the record
/// itself is well-formed), but classifies as `Unprobed`: the probe error must
/// never let the link be followed onto an off-target file, must never be
/// misreported as a confirmed-dead `Stale` verdict the probe never reached, and
/// must never abort the whole scan either.
#[cfg(unix)]
#[test]
fn symlink_lock_target_is_refused_at_open_time() {
    use std::os::unix::fs::symlink;

    let dir = scratch("symlink-lock");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // A decoy the symlink would redirect the probe onto, and a symlink named like
    // a valid lock file pointing at it.
    let decoy = dir.join("decoy-target");
    fs::write(&decoy, b"decoy").expect("write the decoy target");
    let link = dir.join("run-symlink-0000.lock");
    symlink(&decoy, &link).expect("create the symlink lock file");

    // The name itself is a valid simple `.lock` name.
    assert!(is_simple_lock_file_name("run-symlink-0000.lock"));

    write_record(&dir, "run-symlink-0000", "linked", "run-symlink-0000.lock");

    // The open refuses to follow the symlink, so the probe errors — the entry is
    // still returned (its record is well-formed) but classifies `Unprobed`
    // rather than ever being reported `Live` off a link it never actually
    // locked, or `Stale` (a confirmed-dead claim the probe never established).
    let entries = registry.entries().expect("scan");
    let linked = entries
        .iter()
        .find(|entry| entry.record.run_id == "linked")
        .expect("a probe-failed entry is still returned, not dropped");
    assert_eq!(
        linked.health,
        Health::Unprobed,
        "an unprobeable lock file (symlink) must classify Unprobed, not abort the scan"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The regression this task exists for: a lock file that points at a
/// **directory** rather than a regular file makes the liveness probe's
/// write-open fail with a semantic error (`EISDIR` on Unix, an
/// access/"is a directory"-shaped error on Windows) for *any* user, including
/// root — unlike `chmod 0o000` (see [K-014] in the task's KB section), which a
/// privileged or `CAP_DAC_OVERRIDE` CI runner simply ignores, making that
/// approach a false-green trap. `entries()` must not abort the whole scan over
/// this one unprobeable record: the healthy sibling stays `Live`, and the
/// broken one classifies `Unprobed` rather than disappearing, being
/// misreported as the confirmed-dead `Stale` (the T-206 fix), or taking the
/// scan down with it — the exact misrouting bug T-007 fixed by returning
/// (rather than dropping/aborting on) a probe-failed record in the first place
/// (a stale/broken record no longer fails `inspect`/`cancel`/`kill` routing to a
/// *different*, healthy run_id).
#[test]
fn entries_classifies_an_unprobeable_lock_directory_as_unprobed_without_aborting_the_scan() {
    let dir = scratch("dir-lock");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // A record whose `lock_file` name is well-formed but resolves to a directory,
    // not a file: `OpenOptions::read(true).write(true).open(dir)` fails with a
    // semantic "is a directory" error on every platform and for every user.
    let broken_lock_dir = dir.join("broken.lock");
    fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
    write_record(&dir, "broken", "broken", "broken.lock");

    // A well-formed, live sibling entry alongside the unprobeable one.
    let good = registry
        .register_plain("good", None, SystemTime::now())
        .expect("register the good run");

    let entries = registry.entries().expect("scan must not fail");
    assert_eq!(
        entries.len(),
        2,
        "both the healthy and the unprobeable entry are returned"
    );

    let good_entry = entries
        .iter()
        .find(|entry| entry.record.run_id == "good")
        .expect("the healthy entry is present");
    assert_eq!(
        good_entry.health,
        Health::Live,
        "a healthy sibling must stay Live and not be lost to the neighboring probe error"
    );

    let broken_entry = entries
        .iter()
        .find(|entry| entry.record.run_id == "broken")
        .expect("the unprobeable entry is present, not dropped");
    assert_eq!(
        broken_entry.health,
        Health::Unprobed,
        "a record whose lock probe cannot even open must classify Unprobed, never the confirmed-dead Stale"
    );

    good.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// Prune reaps a confirmed-stale **orphan**: a record whose lock file is already
/// gone (`probe_for_prune` opens it and gets `NotFound` — stale by definition, a
/// successful probe, not an error). The dangling `.json` is deleted; there is no
/// lock file left to delete.
#[test]
fn prune_reaps_a_confirmed_stale_orphan_record() {
    let dir = scratch("prune-orphan");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // A record pointing at a well-formed lock name that does not exist on disk.
    write_record(&dir, "orphan", "orphan", "orphan.lock");
    let record_path = dir.join("orphan.json");
    assert!(record_path.exists(), "the orphan record starts on disk");

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "an orphaned stale record is reaped"
    );
    assert!(!record_path.exists(), "the orphaned record file is deleted");

    let _ = fs::remove_dir_all(&dir);
}

/// Prune reaps a confirmed-stale entry whose runner died abruptly (the released
/// lock is taken by the probe, so both files are deleted) — and a second prune over
/// the now-clean registry is a no-op, not an error.
#[test]
fn prune_reaps_a_stale_entry_with_a_released_lock_and_is_idempotent() {
    let dir = scratch("prune-released");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let registration = registry
        .register_plain("victim", None, SystemTime::now())
        .expect("register run");
    let record_path = registration.record_path().to_owned();
    let lock_path = registration.lock_path().to_owned();

    // Abrupt death: release the lock, leave both files behind.
    registration.simulate_abrupt_death();
    assert!(
        record_path.exists() && lock_path.exists(),
        "the abrupt-death fixture leaves both files on disk"
    );

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the confirmed-stale entry is reaped"
    );
    assert!(
        !record_path.exists() && !lock_path.exists(),
        "both files of a reaped entry are deleted"
    );

    // Nothing left to prune: a repeat pass reaps nothing and does not error.
    assert_eq!(
        registry.prune().expect("a second prune must not fail"),
        PruneOutcome::default(),
        "pruning an already-clean registry is a no-op"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A live entry is **never** reaped, even sitting right beside a confirmed-stale
/// one: the live runner still holds its lock, so the probe reports it live and
/// prune leaves its files alone while reaping the dead sibling. Modelled on
/// [`entries_classifies_an_unprobeable_lock_directory_as_unprobed_without_aborting_the_scan`].
#[test]
fn prune_never_reaps_a_live_entry() {
    let dir = scratch("prune-live");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let now = SystemTime::now();

    let live = registry
        .register_plain("alive", None, now)
        .expect("register the live run");
    let doomed = registry
        .register_plain("dead", None, now)
        .expect("register the doomed run");
    let live_record = live.record_path().to_owned();
    let live_lock = live.lock_path().to_owned();
    let dead_record = doomed.record_path().to_owned();
    let dead_lock = doomed.lock_path().to_owned();

    // Only the second runner dies abruptly; the first keeps holding its lock.
    doomed.simulate_abrupt_death();

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 1,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "exactly the stale entry is reaped and the live one is counted, not touched"
    );
    assert!(
        live_record.exists() && live_lock.exists(),
        "a live entry's files must survive prune untouched"
    );
    assert!(
        !dead_record.exists() && !dead_lock.exists(),
        "the stale sibling's files are reaped"
    );

    // The survivor still scans as the live run.
    let entries = registry.entries().expect("scan");
    assert_eq!(entries.len(), 1, "only the live entry remains");
    assert_eq!(entries[0].record.run_id, "alive");
    assert_eq!(entries[0].health, Health::Live);

    live.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// A record whose lock probe **fails** (here the lock name resolves to a
/// *directory*, so the write-open fails with a semantic EISDIR/access error for
/// any user — the confirmed cross-platform trick from [K-014], never `chmod
/// 0o000`) is **not** reaped: liveness is unknown, not confirmed stale, so prune
/// leaves it in place on every pass. One unprobeable entry never aborts the reap
/// of a healthy stale sibling either.
#[test]
fn prune_leaves_an_unprobeable_entry_in_place() {
    let dir = scratch("prune-unprobeable");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // A well-formed record whose `lock_file` name resolves to a directory: the
    // probe's write-open fails with a semantic error, so `probe_for_prune` returns
    // `Err` — the entry must be kept, not deleted.
    let broken_lock_dir = dir.join("broken.lock");
    fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
    write_record(&dir, "broken", "broken", "broken.lock");

    // A confirmed-stale orphan alongside it, which must still be reaped despite the
    // unprobeable neighbor.
    write_record(&dir, "orphan", "orphan", "orphan.lock");

    let outcome = registry
        .prune()
        .expect("prune must not fail on an unprobeable entry");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 1,
            orphaned_locks: 0,
        },
        "the unprobeable entry is kept and the stale sibling is still reaped"
    );
    assert!(
        dir.join("broken.json").exists(),
        "an unprobeable record is never reaped"
    );
    assert!(
        broken_lock_dir.exists(),
        "the unprobeable entry's lock target is left alone"
    );
    assert!(
        !dir.join("orphan.json").exists(),
        "a healthy stale sibling is still reaped past the unprobeable one"
    );

    // Repeated prune keeps leaving the unprobeable entry — at any number of runs.
    assert_eq!(
        registry.prune().expect("a second prune must not fail"),
        PruneOutcome {
            pruned: 0,
            live: 0,
            unprobed: 1,
            orphaned_locks: 0,
        },
        "the unprobeable entry is still kept on a repeat pass"
    );
    assert!(
        dir.join("broken.json").exists(),
        "the unprobeable record survives every prune"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The orphan-lock counterpart to `prune_reaps_a_confirmed_stale_orphan_record`:
/// a lone `.lock` file with **no `.json` sibling at all** — invisible to `scan()`
/// and so unreachable by the paired-record pass no matter how long it sits there
/// — is reaped by `prune`'s second, orphan-lock pass. An unlocked file confirms
/// stale exactly as `probe_for_prune` documents. The fixture is backdated past
/// [`ORPHAN_LOCK_MIN_AGE`] ([R-01]) — a fresh, unlocked lock file must *not* be
/// treated as a candidate at all, since that is exactly the shape of a
/// legitimate reservation's brief pre-lock window; see
/// `prune_never_reaps_a_fresh_unlocked_orphaned_lock_file` below for the
/// *un*-backdated case.
#[test]
fn prune_reaps_a_lone_orphaned_lock_file() {
    let dir = scratch("prune-orphan-lock");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let lock_path = dir.join("orphan.lock");
    fs::write(&lock_path, b"").expect("write the orphaned lock file");
    backdate(&lock_path, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 0,
            live: 0,
            unprobed: 0,
            orphaned_locks: 1,
        },
        "a lone, unlocked .lock file with no .json sibling is reaped as an orphan"
    );
    assert!(!lock_path.exists(), "the orphaned lock file is deleted");

    let _ = fs::remove_dir_all(&dir);
}

/// A `.lock` file **held by a live holder** is never reaped, orphan or not —
/// the same "Live ⇒ never touch" rule the paired-record pass follows. Backdated
/// past [`ORPHAN_LOCK_MIN_AGE`] so this exercises the "old enough, and live" path
/// rather than being excluded by the age floor before it is ever probed.
#[test]
fn prune_never_reaps_a_live_orphaned_lock_file() {
    let dir = scratch("prune-orphan-live");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let lock_path = dir.join("orphan.lock");
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .expect("create the orphaned lock file");
    assert!(
        platform::try_lock_exclusive(&held).expect("take the lock"),
        "a fresh file must not already be locked"
    );
    backdate(&lock_path, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 0,
            live: 1,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "a lock held by a live holder must never be reaped, orphan or not"
    );
    assert!(
        lock_path.exists(),
        "the live-held orphaned lock file survives prune"
    );

    drop(held);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn prune_treats_a_dangling_json_symlink_as_an_existing_sibling() {
    use std::os::unix::fs::symlink;

    let dir = scratch("prune-dangling-json-sibling");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let lock_path = dir.join("paired.lock");
    let json_path = dir.join("paired.json");
    fs::write(&lock_path, b"").expect("write the lock file");
    symlink(dir.join("missing-target"), &json_path).expect("create dangling record symlink");
    backdate(&lock_path, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome::default(),
        "a dangling record symlink is still an existing sibling, so its lock is not orphaned"
    );
    assert!(lock_path.exists(), "the paired lock survives prune");
    assert!(
        fs::symlink_metadata(&json_path).is_ok(),
        "the dangling record symlink survives prune"
    );

    let _ = fs::remove_file(&json_path);
    let _ = fs::remove_dir_all(&dir);
}

/// An orphaned `.lock` whose probe **fails** — here the name resolves to a
/// directory rather than a regular file, the same cross-platform [K-014] trick
/// used for the paired-record probe-error tests — is left in place, not deleted:
/// liveness is unknown, not confirmed stale. Backdated past
/// [`ORPHAN_LOCK_MIN_AGE`] so this exercises the "old enough, but unprobeable"
/// path rather than being excluded by the age floor before it is ever probed.
#[test]
fn prune_leaves_an_unprobeable_orphaned_lock_file_in_place() {
    let dir = scratch("prune-orphan-unprobeable");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let broken = dir.join("broken.lock");
    fs::create_dir(&broken).expect("create the directory the lock name resolves to");
    backdate(&broken, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 0,
            live: 0,
            unprobed: 1,
            orphaned_locks: 0,
        },
        "an unprobeable orphaned lock is kept in place, not deleted"
    );
    assert!(
        broken.exists(),
        "the unprobeable orphaned lock's target is left alone"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// [R-01] regression: a `.lock` file with no `.json` sibling that is younger than
/// [`ORPHAN_LOCK_MIN_AGE`] must not be touched by `prune`'s orphan-lock pass at
/// all, even though it is unlocked and would otherwise read as a textbook
/// "confirmed stale, no live holder" orphan. This is exactly the shape
/// `Registry::reserve_entry` produces for the brief window between `create_new`
/// and taking its own lock — before the age floor, a concurrent `prune` racing
/// that window could reap a legitimate, in-flight reservation's lock file out
/// from under it.
#[test]
fn prune_never_reaps_a_fresh_unlocked_orphaned_lock_file() {
    let dir = scratch("prune-orphan-fresh");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let lock_path = dir.join("orphan.lock");
    fs::write(&lock_path, b"").expect("write the fresh orphaned lock file");

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome::default(),
        "a lock file younger than ORPHAN_LOCK_MIN_AGE must not even be probed, \
         let alone reaped"
    );
    assert!(
        lock_path.exists(),
        "a fresh, not-yet-aged orphan candidate survives prune"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// [R-01] regression, the `reserve_entry` side of the fix: `platform::lock_path_still_matches`
/// must confirm identity between the still-open lock handle and the current
/// contents of its path — not merely that *some* file exists there. A file
/// removed out from under the held lock (the shape a concurrent `prune` leaves
/// behind after reaping the same path first, see the race in [R-01]'s finding)
/// must read back as a mismatch, not a false positive.
#[test]
fn lock_path_still_matches_detects_a_reaped_lock_file() {
    let dir = scratch("reserve-identity");
    fs::create_dir_all(&dir).expect("create scratch dir");

    let lock_path = dir.join("stem.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .expect("create the lock file");
    assert!(
        platform::try_lock_exclusive(&lock).expect("take the lock"),
        "a fresh file must not already be locked"
    );

    assert!(
        platform::lock_path_still_matches(&lock, &lock_path)
            .expect("identity check must not fail while the file still exists"),
        "the path still resolves to the exact file this handle holds"
    );

    // Simulate a concurrent `prune` winning the race: it deletes the file while
    // holding its own (now-released) lock, exactly as `Registry::prune`'s orphan
    // pass does in its `Reapable` arm.
    fs::remove_file(&lock_path).expect("simulate a concurrent reap");
    assert!(
        !platform::lock_path_still_matches(&lock, &lock_path)
            .expect("a missing path is a definitive mismatch, not an error"),
        "a path whose file has been deleted out from under the held lock must \
         never read back as still matching"
    );

    drop(lock);
    let _ = fs::remove_dir_all(&dir);
}

/// Pruning an empty registry — and a never-created one — is a no-op that returns
/// all-zero counts and never errors, and pruning a missing directory does not
/// create it (prune, like `list`, opens read-only).
#[test]
fn prune_over_a_clean_or_missing_registry_is_a_no_op() {
    let dir = scratch("prune-clean");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    assert_eq!(
        registry.prune().expect("prune an empty registry"),
        PruneOutcome::default(),
        "an empty registry has nothing to prune"
    );
    let _ = fs::remove_dir_all(&dir);

    let missing = scratch("prune-missing");
    assert!(!missing.exists(), "the scratch fixture starts absent");
    let read_only = Registry::open_read_only_in(missing.clone());
    assert_eq!(
        read_only.prune().expect("prune a missing registry"),
        PruneOutcome::default(),
        "a missing registry reads back as empty and prunes nothing"
    );
    assert!(
        !missing.exists(),
        "pruning a missing registry must not create its directory"
    );
}

/// Build the mixed fixture `preview_prune`'s equivalence/non-mutation tests
/// share: a live pair (kept alive by the returned [`Registration`], which the
/// caller must hold for the test's duration), a confirmed-stale pair (released
/// lock, both files left behind), an unprobeable pair (its lock name resolves to
/// a directory, the [K-014] trick), and a confirmed-stale orphaned `.lock` file
/// with no `.json` sibling, backdated past [`ORPHAN_LOCK_MIN_AGE`] so it reads as
/// a genuine orphan rather than a fresh, not-yet-locked reservation.
fn mixed_prune_fixture(dir: &Path, registry: &Registry) -> Registration {
    let live = registry
        .register_plain("alive", None, SystemTime::now())
        .expect("register the live run");
    let doomed = registry
        .register_plain("dead", None, SystemTime::now())
        .expect("register the doomed run");
    doomed.simulate_abrupt_death();

    let broken_lock_dir = dir.join("broken.lock");
    fs::create_dir(&broken_lock_dir)
        .expect("create the directory the unprobeable lock name resolves to");
    write_record(dir, "broken", "broken", "broken.lock");

    let orphan_lock = dir.join("orphan.lock");
    fs::write(&orphan_lock, b"").expect("write the orphaned lock file");
    backdate(&orphan_lock, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

    live
}

/// T-199, the heart of `prune --dry-run`'s safety claim: `preview_prune`'s
/// aggregate tally must exactly match what a following, real `prune` pass over
/// the identical, untouched registry state reports. Run over a fixture that
/// exercises every classification at once (live, confirmed-stale, unprobeable,
/// orphaned lock) — a match on this mix is a much stronger claim than a match on
/// any single case.
#[test]
fn preview_prune_matches_a_real_prune_over_identical_state() {
    let dir = scratch("preview-equivalence");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let live = mixed_prune_fixture(&dir, &registry);

    let expected = PruneOutcome {
        pruned: 1,
        live: 1,
        unprobed: 1,
        orphaned_locks: 1,
    };

    let preview = registry
        .preview_prune()
        .expect("preview_prune must not fail");
    assert_eq!(
        preview.outcome, expected,
        "sanity: the mixed fixture must exercise every classification"
    );

    // The preview must not have touched anything: a real prune run right after it
    // reaps exactly the same tally from the exact same on-disk state.
    let real_outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        preview.outcome, real_outcome,
        "a dry-run preview's aggregate tally must equal a real prune's tally over \
         the identical registry state"
    );

    live.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// One [`snapshot_dir`] entry: name, whether it is a directory, its byte length,
/// a regular non-`.lock` file's exact byte contents (`None` for a directory or a
/// `.lock` file — see [`snapshot_dir`]'s docs), and its mtime.
type DirSnapshotEntry = (String, bool, u64, Option<Vec<u8>>, SystemTime);

/// A snapshot of every entry directly inside `dir` — see [`DirSnapshotEntry`] for
/// the fields — sorted for a deterministic comparison. Used to confirm
/// `preview_prune` mutates nothing: a snapshot taken before and after a preview
/// pass must compare equal.
///
/// A `.lock` file's content is deliberately **not** read here (`None`, like a
/// directory): [`platform::try_lock_exclusive`] on Windows takes a whole-file
/// **mandatory** byte-range lock via `LockFileEx` (unlike POSIX `flock`, which
/// stays purely advisory and never blocks a plain read), so `fs::read`-ing a
/// still-live entry's lock file — e.g. this fixture's held `alive` registration —
/// would spuriously fail with a sharing violation, which is a Windows locking
/// artifact, not evidence `preview_prune` touched anything. Every `.lock` file in
/// this codebase is (and only is ever) an empty marker with no meaningful
/// content, so its length is enough to prove nothing was written to it; its
/// mtime and the directory listing itself already prove nothing was deleted,
/// created, or renamed.
fn snapshot_dir(dir: &Path) -> Vec<DirSnapshotEntry> {
    let mut entries: Vec<DirSnapshotEntry> = fs::read_dir(dir)
        .expect("read the scratch registry directory")
        .filter_map(Result::ok)
        .map(|dir_entry| {
            let name = dir_entry.file_name().to_string_lossy().into_owned();
            let path = dir_entry.path();
            let metadata = dir_entry.metadata().expect("read fixture metadata");
            let is_dir = metadata.is_dir();
            let is_lock = path.extension().and_then(|ext| ext.to_str()) == Some("lock");
            let contents = if is_dir || is_lock {
                None
            } else {
                Some(fs::read(&path).expect("read a fixture file's contents"))
            };
            let modified = metadata.modified().expect("read fixture mtime");
            (name, is_dir, metadata.len(), contents, modified)
        })
        .collect();
    entries.sort_by(|(a, ..), (b, ..)| a.cmp(b));
    entries
}

/// T-199: `preview_prune` must never delete, create, or otherwise modify
/// anything — the same mixed fixture as
/// `preview_prune_matches_a_real_prune_over_identical_state`, but here the proof
/// is a byte-for-byte directory snapshot taken before and after the preview pass,
/// rather than only the aggregate counts.
#[test]
fn preview_prune_leaves_the_registry_byte_for_byte_untouched() {
    let dir = scratch("preview-untouched");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let live = mixed_prune_fixture(&dir, &registry);

    let before = snapshot_dir(&dir);
    let preview = registry
        .preview_prune()
        .expect("preview_prune must not fail");
    let after = snapshot_dir(&dir);

    assert_eq!(
        before, after,
        "preview_prune must leave the registry directory byte-for-byte untouched"
    );
    assert_eq!(
        preview.outcome,
        PruneOutcome {
            pruned: 1,
            live: 1,
            unprobed: 1,
            orphaned_locks: 1,
        },
        "sanity: the mixed fixture must exercise every classification"
    );

    live.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// `preview_prune`'s candidate list identifies exactly the two confirmed-stale
/// entries the mixed fixture contains — a paired record by `run_id`/`started_at`,
/// an orphaned lock by its file name — and none of the live or unprobeable ones.
#[test]
fn preview_prune_candidates_identify_the_confirmed_stale_entries() {
    let dir = scratch("preview-candidates");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let live = mixed_prune_fixture(&dir, &registry);

    let preview = registry
        .preview_prune()
        .expect("preview_prune must not fail");
    assert_eq!(
        preview.candidates.len(),
        2,
        "exactly the confirmed-stale pair and the orphaned lock are candidates: \
         {:?}",
        preview.candidates
    );
    assert!(
        preview.candidates.iter().any(|candidate| matches!(
            candidate,
            PruneCandidate::Entry { run_id, socket_dir, .. }
                if run_id == "dead" && socket_dir.is_none()
        )),
        "the confirmed-stale paired entry is a candidate, and — having published no \
         endpoint — names no control socket to reap with it: {:?}",
        preview.candidates
    );
    assert!(
        preview.candidates.iter().any(|candidate| matches!(
            candidate,
            PruneCandidate::OrphanedLock { lock_file_name } if lock_file_name == "orphan.lock"
        )),
        "the orphaned lock file is a candidate: {:?}",
        preview.candidates
    );

    live.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// `preview_prune` over an empty or missing registry is a no-op that returns an
/// all-zero tally and no candidates, exactly like `prune` — the dry-run
/// counterpart to `prune_over_a_clean_or_missing_registry_is_a_no_op`.
#[test]
fn preview_prune_over_a_clean_or_missing_registry_is_a_no_op() {
    let dir = scratch("preview-clean");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    assert_eq!(
        registry.preview_prune().expect("preview an empty registry"),
        PrunePreview::default(),
        "an empty registry has nothing to preview"
    );
    let _ = fs::remove_dir_all(&dir);

    let missing = scratch("preview-missing");
    assert!(!missing.exists(), "the scratch fixture starts absent");
    let read_only = Registry::open_read_only_in(missing.clone());
    assert_eq!(
        read_only
            .preview_prune()
            .expect("preview a missing registry"),
        PrunePreview::default(),
        "a missing registry reads back as empty and previews nothing"
    );
    assert!(
        !missing.exists(),
        "previewing a missing registry must not create its directory"
    );
}

/// T-207, the shape guard on its own: an `endpoint` is a candidate for the socket
/// reap **only** in the exact form `ControlServer::bind` publishes, and every
/// other form — including the ones a corrupt or adversarial record could carry —
/// yields nothing to delete. Exercised against explicit bases so the verdict does
/// not depend on the host's `TMPDIR`, and covering the boundary cases a
/// hand-rolled path validator is easy to get wrong on (see [K-030]): an empty
/// value, a relative path, `..` segments anywhere, a NUL/control character, a
/// deeper or shallower nesting, a near-miss directory name, and a directory
/// outside the allowed bases entirely.
#[cfg(unix)]
#[test]
fn control_socket_endpoints_are_accepted_only_in_the_published_shape() {
    let bases = [PathBuf::from("/tmp"), PathBuf::from("/var/tmp/scratch")];

    // The real thing: what `unique_token`'s pid-nanos-counter form actually looks
    // like, under either base.
    for (endpoint, expected) in [
        (
            "/tmp/pkc-12345-17a2b3c4d5e-0/c.sock",
            "/tmp/pkc-12345-17a2b3c4d5e-0",
        ),
        ("/var/tmp/scratch/pkc-1/c.sock", "/var/tmp/scratch/pkc-1"),
    ] {
        assert_eq!(
            platform::socket_dir_within(endpoint, &bases),
            Some(PathBuf::from(expected)),
            "the published endpoint shape must be recognized: {endpoint:?}"
        );
    }

    for endpoint in [
        // Empty / relative / not a path this project ever publishes.
        "",
        "c.sock",
        "tmp/pkc-1/c.sock",
        "pkc-1/c.sock",
        // Traversal, anywhere in the value — refused before anything resolves it.
        "/tmp/pkc-1/../pkc-2/c.sock",
        "/tmp/../tmp/pkc-1/c.sock",
        "/tmp/pkc-1/c.sock/..",
        // Normalization-equivalent spellings `Path::components()` would silently
        // erase: a `.` segment, a doubled separator, a trailing separator. None
        // of them is what `bind` publishes, so none is accepted here either.
        "/tmp/./pkc-1/c.sock",
        "/tmp//pkc-1/c.sock",
        "//tmp/pkc-1/c.sock",
        "/tmp/pkc-1/c.sock/",
        // NUL / control characters.
        "/tmp/pkc-1/c.sock\0",
        "/tmp/pkc-1\n/c.sock",
        // Wrong file name, or no file at all.
        "/tmp/pkc-1/other.sock",
        "/tmp/pkc-1/c.sock.bak",
        "/tmp/pkc-1/C.SOCK",
        "/tmp/pkc-1",
        "/tmp/pkc-1/",
        // Wrong directory name: near-miss prefix, wrong case, empty token, or a
        // token carrying characters `unique_token` never mints.
        "/tmp/notpkc-1/c.sock",
        "/tmp/PKC-1/c.sock",
        "/tmp/pkc-/c.sock",
        "/tmp/pkc-1 2/c.sock",
        "/tmp/pkc-1.2/c.sock",
        "/tmp/pkc-1:2/c.sock",
        // Right shape, wrong place: too deep, too shallow, or a base that is not
        // one a control server ever binds in.
        "/tmp/sub/pkc-1/c.sock",
        "/tmp/pkc-1/sub/c.sock",
        "/pkc-1/c.sock",
        "/etc/pkc-1/c.sock",
        "/var/tmp/pkc-1/c.sock",
        "/tmp/c.sock",
        // A Windows named pipe, which is not a filesystem path at all.
        r"\\.\pipe\processkit-cli-1234-abc-0",
    ] {
        assert_eq!(
            platform::socket_dir_within(endpoint, &bases),
            None,
            "an endpoint outside the published shape must yield nothing to delete: \
             {endpoint:?}"
        );
    }
}

/// The anti-drift check between the transport that *publishes* an endpoint and the
/// reaper that consumes it: an endpoint a **real** `ControlServer::bind` just
/// produced must classify as a reap candidate, must name the very directory that
/// bind created, and must actually be removable by the reaper — a real
/// tokio-bound socket, not a hand-built fixture. If the socket's naming, its
/// private directory's prefix, or the bases it is created in ever change on the
/// control side, this fails loudly instead of the reap quietly going silent and
/// the leak this task closes coming back.
///
/// `#[tokio::test]`, not `#[test]`: `UnixListener::bind` needs a reactor (see
/// [K-009]).
#[cfg(unix)]
#[tokio::test]
async fn a_freshly_bound_control_endpoint_is_recognized_and_reapable() {
    let server = crate::control::ControlServer::bind().expect("bind a control server");
    let endpoint = server.endpoint().to_string();

    let candidate = platform::control_socket_dir_to_reap(Some(&endpoint))
        .expect("a freshly published endpoint must classify as a reap candidate");
    assert_eq!(
        candidate.join(crate::control::SOCKET_FILE_NAME),
        PathBuf::from(&endpoint),
        "the classified directory must be the one the socket was bound in"
    );
    assert!(
        candidate.is_dir() && Path::new(&endpoint).exists(),
        "sanity: bind really created the directory and the socket"
    );

    // Reap it exactly as a confirmed-stale record's would be reaped. The socket is
    // still bound here — an abruptly-killed runner's is too, from the filesystem's
    // point of view — and it goes, along with its directory.
    platform::reap_control_socket_dir(&candidate);
    assert!(
        !Path::new(&endpoint).exists(),
        "a real published control socket is unlinked by the reaper"
    );
    assert!(
        !candidate.exists(),
        "its private directory goes with it, leaving nothing behind"
    );

    // The server's own clean-teardown Drop is best-effort and copes with the
    // files already being gone.
    drop(server);
}

/// T-207, the leak this task closes: reaping a confirmed-stale entry also removes
/// the control socket that entry published and the private directory holding it —
/// the other half of what an abruptly-killed runner strands on disk, which no
/// pass ever cleaned up before.
#[cfg(unix)]
#[test]
fn prune_reaps_the_control_socket_a_confirmed_stale_record_published() {
    let dir = scratch("prune-socket-reap");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let (socket_dir, endpoint) = socket_fixture("reap");

    let registration = registry
        .register_plain("victim", Some(&endpoint), SystemTime::now())
        .expect("register run");
    let record_path = registration.record_path().to_owned();
    let lock_path = registration.lock_path().to_owned();
    // Abrupt death: the lock is released, and every file — record, lock, and the
    // socket the clean-teardown `Drop` never got to remove — is left behind.
    registration.simulate_abrupt_death();
    assert!(
        Path::new(&endpoint).exists() && socket_dir.exists(),
        "the abrupt-death fixture leaves the control socket on disk"
    );

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the confirmed-stale entry is reaped"
    );
    assert!(
        !record_path.exists() && !lock_path.exists(),
        "both registry files of the reaped entry are deleted"
    );
    assert!(
        !Path::new(&endpoint).exists(),
        "the control socket the reaped record published is deleted too"
    );
    assert!(
        !socket_dir.exists(),
        "the socket's private directory is reaped with it, not left behind empty"
    );

    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&dir);
}

/// A **live** run's control socket is never touched — the same guarantee the live
/// entry's own files already have. The socket reap runs only inside the
/// confirmed-stale arm, so a live runner keeps the transport its clients are
/// still connecting to.
#[cfg(unix)]
#[test]
fn prune_never_reaps_the_control_socket_of_a_live_entry() {
    let dir = scratch("prune-socket-live");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let (socket_dir, endpoint) = socket_fixture("live");

    let live = registry
        .register_plain("alive", Some(&endpoint), SystemTime::now())
        .expect("register the live run");

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 0,
            live: 1,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the live entry is counted as kept, not reaped"
    );
    assert!(
        Path::new(&endpoint).exists() && socket_dir.exists(),
        "a live run's control socket and its directory must survive prune untouched"
    );

    live.remove();
    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&dir);
}

/// An entry whose liveness probe **fails** keeps its control socket too: liveness
/// is unknown, not confirmed stale, so nothing about that entry is deleted — the
/// socket included. The probe is forced to fail with the cross-platform
/// lock-file-is-a-directory trick from [K-014], never `chmod 0o000`.
#[cfg(unix)]
#[test]
fn prune_leaves_the_control_socket_of_an_unprobeable_entry_alone() {
    let dir = scratch("prune-socket-unprobeable");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let (socket_dir, endpoint) = socket_fixture("unpr");

    fs::create_dir(dir.join("broken.lock"))
        .expect("create the directory the lock name resolves to");
    write_record_with_endpoint(&dir, "broken", "broken", "broken.lock", Some(&endpoint));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 0,
            live: 0,
            unprobed: 1,
            orphaned_locks: 0,
        },
        "an unprobeable entry is kept"
    );
    assert!(
        dir.join("broken.json").exists(),
        "an unprobeable record is never reaped"
    );
    assert!(
        Path::new(&endpoint).exists() && socket_dir.exists(),
        "an unprobeable entry's control socket is never reaped either"
    );

    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&dir);
}

/// The symlink attack the [K-024] `O_NOFOLLOW` discipline exists for, aimed at the
/// socket directory instead of a lock file: a `pkc-…` name inside a base that is
/// really a **symlink** to somewhere else entirely. The name passes the lexical
/// shape check (it is exactly the published form), so only the open-time refusal
/// stands between the reap and the link's target — and it holds: nothing behind
/// the link is deleted, and the link itself is left in place rather than being
/// followed. The record is still reaped, since the endpoint check gates only the
/// extra socket deletion.
#[cfg(unix)]
#[test]
fn prune_refuses_to_follow_a_symlinked_control_socket_directory() {
    use std::os::unix::fs::symlink;

    let dir = scratch("prune-socket-symlink");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // What the link points at: a directory holding a socket named exactly like a
    // published one, plus an unrelated bystander file.
    let decoy = off_base_dir("dcy");
    let _ = fs::remove_dir_all(&decoy);
    fs::create_dir_all(&decoy).expect("create the decoy target directory");
    let decoy_socket = bind_socket_in(&decoy);
    let bystander = decoy.join("bystander");
    fs::write(&bystander, b"not yours to delete").expect("write the bystander file");

    // The endpoint: a perfectly-shaped `<base>/pkc-<token>/c.sock`, whose
    // directory component is a symlink onto the decoy.
    let link = socket_dir_path("link");
    let _ = fs::remove_dir_all(&link);
    symlink(&decoy, &link).expect("create the symlinked socket directory");
    let endpoint = link.join(crate::control::SOCKET_FILE_NAME);
    let endpoint = endpoint.to_str().expect("a UTF-8 endpoint");
    assert!(
        platform::control_socket_dir_to_reap(Some(endpoint)).is_some(),
        "sanity: the lexical shape check passes, so only the open-time refusal \
         can stop this reap"
    );

    write_record_with_endpoint(&dir, "linked", "linked", "linked.lock", Some(endpoint));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the record itself is still reaped — the endpoint check gates only the \
         socket deletion"
    );
    assert!(
        Path::new(&decoy_socket).exists(),
        "the symlink's target must not be followed: the socket behind it survives"
    );
    assert!(
        bystander.exists() && decoy.exists(),
        "nothing behind the symlink is deleted"
    );
    assert!(
        fs::symlink_metadata(&link).is_ok(),
        "the symlink itself is left in place, not resolved and reaped"
    );

    let _ = fs::remove_file(&link);
    let _ = fs::remove_dir_all(&decoy);
    let _ = fs::remove_dir_all(&dir);
}

/// Even inside a genuine, validated `pkc-…` directory, only a real **socket** is
/// unlinked: a regular file planted under the socket's name is refused, and the
/// directory holding it is then left alone too (rather than being emptied of
/// whatever happens to sit there). The record is reaped as usual.
#[cfg(unix)]
#[test]
fn prune_refuses_to_delete_an_endpoint_that_is_not_a_socket() {
    let dir = scratch("prune-socket-not-a-socket");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let socket_dir = socket_dir_path("file");
    let _ = fs::remove_dir_all(&socket_dir);
    fs::create_dir(&socket_dir).expect("create the private control-socket directory");
    let planted = socket_dir.join(crate::control::SOCKET_FILE_NAME);
    fs::write(&planted, b"a regular file, not a socket").expect("plant the decoy file");
    let endpoint = planted.to_str().expect("a UTF-8 endpoint").to_string();

    write_record_with_endpoint(&dir, "planted", "planted", "planted.lock", Some(&endpoint));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the record is reaped whatever its endpoint turned out to be"
    );
    assert!(
        planted.exists(),
        "a file that is not a socket is refused, never deleted"
    );
    assert!(
        socket_dir.exists(),
        "the directory still holding the refused file is left alone too"
    );

    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&dir);
}

/// A well-formed-looking endpoint **outside** the base directories a control
/// server ever binds in deletes nothing at all: the record is reaped, and the
/// directory it pointed at — socket, sibling file, and the directory itself —
/// survives untouched. This is the property that keeps a corrupt or hand-edited
/// record from steering the reap at an arbitrary path.
#[cfg(unix)]
#[test]
fn prune_ignores_an_endpoint_outside_the_control_socket_bases() {
    let dir = scratch("prune-socket-outside");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // `<temp>/<other>/pkc-1/c.sock`: the right *shape*, one level too deep to be a
    // directory `ControlServer::bind` created.
    let outside = off_base_dir("off");
    let _ = fs::remove_dir_all(&outside);
    let elsewhere = outside.join("pkc-1");
    fs::create_dir_all(&elsewhere).expect("create the off-base directory");
    let socket = bind_socket_in(&elsewhere);
    let bystander = elsewhere.join("bystander");
    fs::write(&bystander, b"not yours to delete").expect("write the bystander file");
    assert!(
        platform::control_socket_dir_to_reap(Some(&socket)).is_none(),
        "sanity: an endpoint outside the published bases is not a candidate"
    );

    write_record_with_endpoint(&dir, "offbase", "offbase", "offbase.lock", Some(&socket));

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the record itself is reaped as usual"
    );
    assert!(
        Path::new(&socket).exists() && bystander.exists() && elsewhere.exists(),
        "nothing outside the published socket bases is deleted"
    );

    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&dir);
}

/// `prune --dry-run`'s side of T-207: the preview names the socket directory a
/// real reap would remove — and stays silent for an entry whose endpoint that
/// reap would refuse — without removing either. The following real prune then
/// does exactly what the preview said: one socket reaped, the refused one
/// untouched.
#[cfg(unix)]
#[test]
fn preview_prune_reports_the_control_socket_it_would_reap_and_removes_nothing() {
    let dir = scratch("preview-socket");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    // One confirmed-stale entry whose endpoint the reap accepts...
    let (socket_dir, endpoint) = socket_fixture("pvw");
    write_record_with_endpoint(
        &dir,
        "reapable",
        "reapable",
        "reapable.lock",
        Some(&endpoint),
    );

    // ...and one whose endpoint it refuses (right shape, wrong place).
    let outside = off_base_dir("pvw");
    let _ = fs::remove_dir_all(&outside);
    let elsewhere = outside.join("pkc-1");
    fs::create_dir_all(&elsewhere).expect("create the off-base directory");
    let refused = bind_socket_in(&elsewhere);
    write_record_with_endpoint(&dir, "refused", "refused", "refused.lock", Some(&refused));

    let preview = registry
        .preview_prune()
        .expect("preview_prune must not fail");
    assert_eq!(
        preview.outcome,
        PruneOutcome {
            pruned: 2,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "both records are confirmed-stale candidates"
    );
    assert!(
        preview.candidates.iter().any(|candidate| matches!(
            candidate,
            PruneCandidate::Entry { run_id, socket_dir: Some(reported), .. }
                if run_id == "reapable" && Path::new(reported) == socket_dir
        )),
        "the preview names the socket directory a real reap would remove: {:?}",
        preview.candidates
    );
    assert!(
        preview.candidates.iter().any(|candidate| matches!(
            candidate,
            PruneCandidate::Entry { run_id, socket_dir, .. }
                if run_id == "refused" && socket_dir.is_none()
        )),
        "the preview names no socket for an endpoint the reap would refuse: {:?}",
        preview.candidates
    );
    assert!(
        Path::new(&endpoint).exists() && Path::new(&refused).exists(),
        "a preview must not delete either socket"
    );

    // The real pass now does exactly what the preview described.
    registry.prune().expect("prune must not fail");
    assert!(
        !Path::new(&endpoint).exists() && !socket_dir.exists(),
        "the previewed socket directory is what the real reap removes"
    );
    assert!(
        Path::new(&refused).exists(),
        "the endpoint the preview reported no candidate for is still not deleted"
    );

    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&dir);
}

/// The Windows side of T-207: a named-pipe endpoint is never a socket candidate —
/// the pipe lives in the kernel object namespace and disappears with its creator,
/// so there is no filesystem leftover to classify. The preview reports no socket
/// directory and the reap deletes exactly the two registry files it always did.
#[cfg(windows)]
#[test]
fn a_named_pipe_endpoint_is_never_a_control_socket_candidate() {
    let dir = scratch("prune-socket-pipe");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let endpoint = r"\\.\pipe\processkit-cli-1234-17a2b3c4d5e-0";
    write_record_with_endpoint(&dir, "piped", "piped", "piped.lock", Some(endpoint));

    let preview = registry
        .preview_prune()
        .expect("preview_prune must not fail");
    assert!(
        preview.candidates.iter().any(|candidate| matches!(
            candidate,
            PruneCandidate::Entry { run_id, socket_dir, .. }
                if run_id == "piped" && socket_dir.is_none()
        )),
        "a named-pipe endpoint names no directory to reap: {:?}",
        preview.candidates
    );

    let outcome = registry.prune().expect("prune must not fail");
    assert_eq!(
        outcome,
        PruneOutcome {
            pruned: 1,
            live: 0,
            unprobed: 0,
            orphaned_locks: 0,
        },
        "the record is reaped exactly as it was before the socket reap existed"
    );
    assert!(
        !dir.join("piped.json").exists(),
        "the confirmed-stale record is gone"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The `wait` read path end to end on the ordinary lifecycle: a registered run
/// probes as [`RunStatus::Live`] while its runner holds the lock, and as
/// [`RunStatus::Finished`] the moment its clean exit removes the entry.
#[test]
fn probe_run_tracks_a_run_from_live_to_finished() {
    let dir = scratch("probe-run-lifecycle");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("waited", None, SystemTime::now())
        .expect("register run");

    assert_eq!(
        registry.probe_run("waited").expect("probe"),
        RunStatus::Live,
        "a run whose runner holds its lock is live"
    );

    registration.remove();
    assert_eq!(
        registry.probe_run("waited").expect("probe"),
        RunStatus::Finished,
        "a clean exit removes the entry, so the run reads as finished"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The documented conflation: a `run_id` with no record at all is
/// [`RunStatus::Finished`], because a run that exits cleanly deletes its own
/// entry — "never registered" and "already finished and cleaned up" are the same
/// observation, and the registry keeps no history that could separate them.
#[test]
fn probe_run_reports_an_unknown_run_id_as_finished() {
    let dir = scratch("probe-run-unknown");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    assert_eq!(
        registry.probe_run("never-registered").expect("probe"),
        RunStatus::Finished,
        "an id nobody registered is indistinguishable from one already cleaned up"
    );

    // The same answer with an unrelated live run in the registry: matching is by
    // `run_id`, so another run's liveness never leaks into this one's verdict.
    let other = registry
        .register_plain("someone-else", None, SystemTime::now())
        .expect("register an unrelated run");
    assert_eq!(
        registry.probe_run("never-registered").expect("probe"),
        RunStatus::Finished,
        "an unrelated live run must not make an unknown id look live"
    );

    other.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// An abruptly-killed runner leaves both files on disk, yet the run is over: the
/// released lock makes the entry confirmed-stale, so `wait` stops waiting. The
/// files are left exactly where they are — `probe_run` is a query, not a reaper
/// (that is `prune`'s job).
#[test]
fn probe_run_reports_a_stale_leftover_as_finished_without_reaping_it() {
    let dir = scratch("probe-run-stale");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("crashed", None, SystemTime::now())
        .expect("register run");
    let record_path = registration.record_path().to_owned();
    let lock_path = registration.lock_path().to_owned();

    registration.simulate_abrupt_death();

    assert_eq!(
        registry.probe_run("crashed").expect("probe"),
        RunStatus::Finished,
        "a leftover entry whose lock is released means the run is over"
    );
    assert!(
        record_path.exists() && lock_path.exists(),
        "a read-only probe must leave the stale entry's files on disk"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Two live runs under one `run_id` (the registry never enforces uniqueness) make
/// the id name no single run, so the verdict is [`RunStatus::Ambiguous`] with the
/// live count — never a silent pick of whichever entry the scan yielded first.
///
/// One of the duplicates deliberately publishes **no endpoint**: liveness is
/// counted by the identity predicate (`run_id`) alone, before any secondary
/// attribute, which is exactly the undercount [K-016] found in `src/control.rs`
/// when the two were folded into one filter pass. Here the point is even sharper
/// than there — `wait` never needs an endpoint at all, so an endpoint-less live
/// run is an entirely ordinary run to wait for, not a lesser one.
#[test]
fn probe_run_reports_ambiguity_counting_even_an_endpoint_less_duplicate() {
    let dir = scratch("probe-run-ambiguous");
    let registry = Registry::open_in(dir.clone()).expect("open registry");
    let now = SystemTime::now();

    let with_endpoint = registry
        .register_plain("dup", Some("endpoint-a"), now)
        .expect("register the first duplicate");
    let without_endpoint = registry
        .register_plain("dup", None, now)
        .expect("register the second duplicate");

    assert_eq!(
        registry.probe_run("dup").expect("probe"),
        RunStatus::Ambiguous { live: 2 },
        "two live runs under one id is an ambiguity, counted by run_id alone"
    );

    // Once one of them ends, the id names a single run again and the wait can
    // resume normally — the ambiguity is a property of the moment, not a curse
    // on the id.
    without_endpoint.remove();
    assert_eq!(
        registry.probe_run("dup").expect("probe"),
        RunStatus::Live,
        "with one duplicate gone the surviving run is unambiguously live"
    );

    with_endpoint.remove();
    let _ = fs::remove_dir_all(&dir);
}

/// The [K-024] property this method exists for: a matching record whose liveness
/// **cannot be probed** (its lock name resolves to a *directory*, so the
/// write-open fails with a semantic error for any user — the cross-platform trick
/// from [K-014], never `chmod 0o000`) must read as [`RunStatus::Unprobed`], never
/// as [`RunStatus::Finished`]. Fabricating "finished" from a probe that never
/// actually ran would have `wait` announce a live run as over.
#[test]
fn probe_run_reports_an_unprobeable_record_as_unprobed_not_finished() {
    let dir = scratch("probe-run-unprobeable");
    let registry = Registry::open_in(dir.clone()).expect("open registry");

    let broken_lock_dir = dir.join("broken.lock");
    fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
    write_record(&dir, "broken", "opaque", "broken.lock");

    assert_eq!(
        registry.probe_run("opaque").expect("probe"),
        RunStatus::Unprobed,
        "an unprobeable record leaves the run's fate unknown, not confirmed over"
    );
    // `entries()` reaches the same "not confirmed" verdict independently, via
    // `Health::Unprobed` (T-206) — the two methods agree on this record's health
    // even though neither is built on the other's scan.
    let entries = registry.entries().expect("scan");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].health,
        Health::Unprobed,
        "entries() classifies an unprobeable entry Unprobed, agreeing with probe_run"
    );

    // A confirmed-live record under the same id outranks the unknown one: there
    // is something definite to wait for.
    let live = registry
        .register_plain("opaque", None, SystemTime::now())
        .expect("register a live run under the same id");
    assert_eq!(
        registry.probe_run("opaque").expect("probe"),
        RunStatus::Live,
        "a confirmed-live record is a stronger fact than an unprobeable one"
    );

    live.remove();
    let _ = fs::remove_dir_all(&dir);
}
