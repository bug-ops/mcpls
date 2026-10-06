//! Pulled slots of the diagnostics cache: provenance, dedupe, change
//! detection, races and budgets.

use lsp_types::{Code, DiagnosticSeverity, Position, Range};

use super::bounds::*;
use super::pulled_index::*;
use super::*;

fn server() -> ServerId {
    ServerId::from("rust")
}

fn file() -> Uri {
    Uri::from("file:///ws/main.rs")
}

fn pushed_via(source: &str) -> PublishedDiagnosticsUri {
    PublishedDiagnosticsUri::for_test(Uri::from(format!("file:///ws/{source}")), file())
}

fn diag(
    line: u32,
    severity: DiagnosticSeverity,
    code: Option<Code>,
    message: &str,
) -> LspDiagnostic {
    LspDiagnostic {
        range: Range {
            start: Position { line, character: 0 },
            end: Position { line, character: 5 },
        },
        severity: Some(severity),
        message: message.to_owned().into(),
        code,
        ..LspDiagnostic::default()
    }
}

fn error(line: u32, message: &str) -> LspDiagnostic {
    diag(line, DiagnosticSeverity::Error, None, message)
}

fn coded(line: u32, code: &str, message: &str) -> LspDiagnostic {
    diag(
        line,
        DiagnosticSeverity::Error,
        Some(Code::String(code.to_owned())),
        message,
    )
}

fn bounded(items: Vec<LspDiagnostic>) -> BoundedDiagnostics {
    BoundedDiagnostics::new(&file(), items)
}

fn push(cache: &mut NotificationCache, version: Option<i32>, items: Vec<LspDiagnostic>) {
    cache.store_published_diagnostics(&server(), &pushed_via("main.rs"), version, items);
}

fn pull_for(
    cache: &mut NotificationCache,
    target: &Uri,
    version: i32,
    items: Vec<LspDiagnostic>,
) -> PullWrite {
    let stamp = cache.begin_pull(&server(), version);
    cache.store_pulled_diagnostics(
        &server(),
        target,
        stamp,
        VersionCheck::Current,
        BoundedDiagnostics::new(target, items),
    )
}

fn pull(cache: &mut NotificationCache, items: Vec<LspDiagnostic>) -> PullWrite {
    pull_for(cache, &file(), 1, items)
}

fn merged(cache: &NotificationCache) -> Option<DiagnosticInfo> {
    cache.diagnostic_sources(&file()).merge()
}

fn messages(cache: &NotificationCache) -> Vec<String> {
    merged(cache)
        .map(|info| {
            info.diagnostics
                .iter()
                .map(|d| message_as_str(&d.message).to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// What a read of the file returns after `write`, computed as the translator does.
fn outcome(cache: &NotificationCache, write: PullWrite) -> ChangeOutcome {
    match write {
        PullWrite::Stored {
            slot: SlotChange::Identical,
            ..
        } => ChangeOutcome::Unchanged,
        PullWrite::Stored {
            slot: SlotChange::Replaced { before },
            ..
        } => ChangeOutcome::of(before.merge().as_ref(), merged(cache).as_ref()),
        PullWrite::Discarded { reason, .. } => panic!("pull was discarded: {reason:?}"),
    }
}

fn discarded(write: &PullWrite) -> Discard {
    match write {
        PullWrite::Discarded { reason, .. } => *reason,
        PullWrite::Stored { .. } => panic!("pull was stored"),
    }
}

#[test]
fn test_push_and_pull_do_not_erase_each_other() {
    let mut cache = NotificationCache::new();
    push(
        &mut cache,
        Some(1),
        vec![diag(
            9,
            DiagnosticSeverity::Warning,
            None,
            "flycheck warning",
        )],
    );

    let write = pull(&mut cache, vec![]);
    assert_eq!(outcome(&cache, write), ChangeOutcome::Unchanged);
    assert_eq!(messages(&cache), ["flycheck warning"]);

    let write = pull(&mut cache, vec![error(1, "E0308")]);
    assert_eq!(outcome(&cache, write), ChangeOutcome::Changed);
    assert_eq!(messages(&cache), ["E0308", "flycheck warning"]);

    push(
        &mut cache,
        Some(1),
        vec![diag(9, DiagnosticSeverity::Warning, None, "other")],
    );
    assert_eq!(messages(&cache), ["E0308", "other"]);
    cache.assert_consistent();
}

#[test]
fn test_pulled_slot_is_visible_to_every_read_path() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "E0308")]));

    assert!(cache.has_diagnostics(&file()));
    assert!(cache.is_listen_replayable(&file()));
    assert!(cache.diagnostics(&file()).is_none());
    assert_eq!(cache.diagnostics_owner(&file()), Some(&server()));
    assert_eq!(messages(&cache), ["E0308"]);
    assert_eq!(merged(&cache).unwrap().version, Some(1));
}

#[test]
fn test_diagnostics_returns_the_pushed_slot_only() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "pulled")]));
    push(&mut cache, Some(1), vec![error(2, "pushed")]);

    let info = cache.diagnostics(&file()).unwrap();

    assert_eq!(info.diagnostics.len(), 1);
    assert_eq!(message_as_str(&info.diagnostics[0].message), "pushed");
}

#[test]
fn test_same_code_within_three_lines_collapses_to_the_pulled_item() {
    let mut cache = NotificationCache::new();
    push(
        &mut cache,
        Some(1),
        vec![coded(96, "E0046", "rendered by rustc")],
    );
    drop(pull(&mut cache, vec![coded(98, "E0046", "terse")]));

    assert_eq!(messages(&cache), ["terse"]);
}

#[test]
fn test_same_code_far_apart_keeps_both() {
    let mut cache = NotificationCache::new();
    push(&mut cache, Some(1), vec![coded(49, "E0308", "second")]);
    drop(pull(&mut cache, vec![coded(5, "E0308", "first")]));

    assert_eq!(messages(&cache), ["first", "second"]);
}

#[test]
fn test_without_a_code_only_full_equality_collapses() {
    let mut cache = NotificationCache::new();
    push(
        &mut cache,
        Some(1),
        vec![error(1, "expected `i32`, found `&str`"), error(2, "same")],
    );
    drop(pull(
        &mut cache,
        vec![error(1, "mismatched types"), error(2, "same")],
    ));

    assert_eq!(
        messages(&cache),
        ["mismatched types", "expected `i32`, found `&str`", "same"]
    );
}

#[test]
fn test_dedupe_normalizes_missing_severity_and_integer_codes() {
    let mut cache = NotificationCache::new();
    let mut pushed = diag(1, DiagnosticSeverity::Information, Some(Code::Int(7)), "x");
    pushed.severity = None;
    push(&mut cache, Some(1), vec![pushed]);
    drop(pull(
        &mut cache,
        vec![diag(
            1,
            DiagnosticSeverity::Information,
            Some(Code::String("7".to_owned())),
            "y",
        )],
    ));

    assert_eq!(messages(&cache), ["y"]);
}

#[test]
fn test_pulled_items_are_never_collapsed_among_themselves() {
    let mut cache = NotificationCache::new();
    drop(pull(
        &mut cache,
        vec![
            coded(1, "E1", "a"),
            coded(1, "E1", "a"),
            coded(2, "E1", "b"),
        ],
    ));

    assert_eq!(messages(&cache), ["a", "a", "b"]);
}

#[test]
fn test_equal_ranges_keep_the_pulled_item_first() {
    let mut cache = NotificationCache::new();
    push(&mut cache, Some(1), vec![error(1, "pushed")]);
    drop(pull(&mut cache, vec![error(1, "pulled")]));

    assert_eq!(messages(&cache), ["pulled", "pushed"]);
}

#[test]
fn test_a_hundred_identical_pulls_change_nothing_after_the_first() {
    let mut cache = NotificationCache::new();
    let items = || vec![error(1, "a"), error(2, "b")];
    let write = pull(&mut cache, items());
    assert_eq!(outcome(&cache, write), ChangeOutcome::Changed);

    for _ in 0..100 {
        let write = pull(&mut cache, items());
        assert_eq!(outcome(&cache, write), ChangeOutcome::Unchanged);
    }
    cache.assert_consistent();
}

#[test]
fn test_a_reordered_identical_pull_is_unchanged() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "a"), error(2, "b")]));

    let write = pull(&mut cache, vec![error(2, "b"), error(1, "a")]);

    assert_eq!(outcome(&cache, write), ChangeOutcome::Unchanged);
}

#[test]
fn test_first_pull_duplicating_the_pushed_items_is_unchanged() {
    let mut cache = NotificationCache::new();
    push(&mut cache, Some(1), vec![coded(1, "E1", "same")]);

    let write = pull(&mut cache, vec![coded(1, "E1", "same")]);

    assert_eq!(outcome(&cache, write), ChangeOutcome::Unchanged);
}

#[test]
fn test_clean_pull_of_an_unseen_file_is_a_change_but_not_with_pushes() {
    let mut cache = NotificationCache::new();
    let write = pull(&mut cache, vec![]);
    assert_eq!(outcome(&cache, write), ChangeOutcome::Changed);

    let mut with_push = NotificationCache::new();
    push(&mut with_push, Some(1), vec![error(1, "e")]);
    let write = pull(&mut with_push, vec![]);
    assert_eq!(outcome(&with_push, write), ChangeOutcome::Unchanged);
}

#[test]
fn test_fixing_the_only_error_is_a_change() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "E0308")]));

    let write = pull(&mut cache, vec![]);

    assert_eq!(outcome(&cache, write), ChangeOutcome::Changed);
    assert!(messages(&cache).is_empty());
    assert!(merged(&cache).is_some());
}

#[test]
fn test_older_ticket_is_discarded_and_the_slot_keeps_the_newer_report() {
    let mut cache = NotificationCache::new();
    let older = cache.begin_pull(&server(), 1);
    let newer = cache.begin_pull(&server(), 1);
    let store = |cache: &mut NotificationCache, stamp, message: &str| {
        cache.store_pulled_diagnostics(
            &server(),
            &file(),
            stamp,
            VersionCheck::Current,
            bounded(vec![error(1, message)]),
        )
    };

    drop(store(&mut cache, newer, "newer"));
    let write = store(&mut cache, older, "older");

    assert_eq!(discarded(&write), Discard::OlderTicket);
    assert_eq!(messages(&cache), ["newer"]);
}

#[test]
fn test_a_clear_between_issue_and_store_discards_the_pull() {
    let mut cache = NotificationCache::new();
    let stamp = cache.begin_pull(&server(), 1);

    assert!(cache.clear_server_diagnostics(&server()).is_empty());
    let write = cache.store_pulled_diagnostics(
        &server(),
        &file(),
        stamp,
        VersionCheck::Current,
        bounded(vec![error(1, "from the old process")]),
    );

    assert_eq!(discarded(&write), Discard::ServerCleared);
    assert!(!cache.has_diagnostics(&file()));
    cache.assert_consistent();
}

#[test]
fn test_a_clear_of_another_server_keeps_the_pull() {
    let mut cache = NotificationCache::new();
    let stamp = cache.begin_pull(&server(), 1);
    cache.clear_server_diagnostics(&ServerId::from("other"));

    let write = cache.store_pulled_diagnostics(
        &server(),
        &file(),
        stamp,
        VersionCheck::Current,
        bounded(vec![]),
    );

    assert!(matches!(write, PullWrite::Stored { .. }));
}

#[test]
fn test_a_moved_version_discards_the_pull() {
    let mut cache = NotificationCache::new();
    let stamp = cache.begin_pull(&server(), 1);

    let write = cache.store_pulled_diagnostics(
        &server(),
        &file(),
        stamp,
        VersionCheck::Moved,
        bounded(vec![error(1, "stale")]),
    );

    assert_eq!(discarded(&write), Discard::VersionMoved);
    assert!(!cache.has_diagnostics(&file()));
}

#[test]
fn test_a_discarded_pull_hands_its_items_back() {
    let mut cache = NotificationCache::new();
    let stamp = cache.begin_pull(&server(), 3);
    let write = cache.store_pulled_diagnostics(
        &server(),
        &file(),
        stamp,
        VersionCheck::Moved,
        bounded(vec![error(1, "stale")]),
    );
    let PullWrite::Discarded { items, .. } = write else {
        panic!("pull was stored");
    };

    let overlay = cache
        .diagnostic_sources(&file())
        .with_pulled(&file(), Some(stamp.version()), items)
        .merge()
        .unwrap();

    assert_eq!(overlay.version, Some(3));
    assert_eq!(overlay.diagnostics.len(), 1);
}

#[test]
fn test_a_stale_pushed_version_does_not_block_the_pull() {
    let mut cache = NotificationCache::new();
    push(&mut cache, Some(900), vec![error(1, "left over")]);

    let write = pull(&mut cache, vec![error(2, "fresh")]);

    assert!(matches!(write, PullWrite::Stored { .. }));
    assert_eq!(messages(&cache), ["left over", "fresh"]);
}

#[test]
fn test_a_newer_push_evicts_the_pulled_slot() {
    let mut cache = NotificationCache::new();
    drop(pull_for(&mut cache, &file(), 4, vec![error(1, "pulled")]));

    push(&mut cache, Some(5), vec![error(2, "pushed")]);

    assert_eq!(messages(&cache), ["pushed"]);
    cache.assert_consistent();
}

#[test]
fn test_an_equal_or_unversioned_push_keeps_the_pulled_slot() {
    let mut cache = NotificationCache::new();
    drop(pull_for(&mut cache, &file(), 4, vec![error(1, "pulled")]));

    push(&mut cache, Some(4), vec![error(2, "equal")]);
    assert_eq!(messages(&cache), ["pulled", "equal"]);

    push(&mut cache, None, vec![error(2, "unversioned")]);
    assert_eq!(messages(&cache), ["pulled", "unversioned"]);
}

#[test]
fn test_merged_version_prefers_the_pushed_canonical_then_the_pulled() {
    let mut cache = NotificationCache::new();
    drop(pull_for(&mut cache, &file(), 4, vec![]));
    assert_eq!(merged(&cache).unwrap().version, Some(4));

    cache.store_published_diagnostics(&server(), &pushed_via("alias.rs"), None, vec![]);
    assert_eq!(merged(&cache).unwrap().version, Some(4));

    push(&mut cache, Some(4), vec![]);
    assert_eq!(merged(&cache).unwrap().version, Some(4));
    push(&mut cache, None, vec![]);
    assert_eq!(merged(&cache).unwrap().version, Some(4));
}

#[test]
fn test_a_pulled_slot_does_not_count_toward_the_source_cap() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "pulled")]));
    for n in 0..MAX_SOURCES_PER_FILE {
        cache.store_published_diagnostics(
            &server(),
            &pushed_via(&format!("a{n}.rs")),
            None,
            vec![error(10, &format!("alias{n}"))],
        );
    }
    assert_eq!(cache.diagnostics_count(), MAX_SOURCES_PER_FILE + 1);

    cache.store_published_diagnostics(&server(), &pushed_via("extra.rs"), None, vec![]);

    assert_eq!(cache.diagnostics_count(), MAX_SOURCES_PER_FILE + 1);
    assert!(
        cache
            .diagnostics(&Uri::from("file:///ws/extra.rs"))
            .is_none()
    );
    cache.assert_consistent();
}

#[test]
fn test_canonical_push_is_admitted_beside_a_pulled_slot_and_full_aliases() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "pulled")]));
    for n in 0..MAX_SOURCES_PER_FILE {
        cache.store_published_diagnostics(
            &server(),
            &pushed_via(&format!("a{n}.rs")),
            None,
            vec![],
        );
    }

    push(&mut cache, Some(1), vec![error(5, "canonical")]);

    assert_eq!(messages(&cache), ["pulled", "canonical"]);
    assert_eq!(cache.diagnostics_count(), MAX_SOURCES_PER_FILE + 1);
    cache.assert_consistent();
}

#[test]
fn test_clearing_a_server_removes_its_pulled_slots() {
    let mut cache = NotificationCache::new();
    drop(pull(&mut cache, vec![error(1, "pulled")]));
    push(&mut cache, Some(1), vec![]);

    let cleared = cache.clear_server_diagnostics(&server());

    assert_eq!(cleared, [DiagnosticsKey::of(&file())]);
    assert!(cache.files.is_empty());
    assert!(!cache.has_diagnostics(&file()));
    cache.assert_consistent();
}

#[test]
fn test_pulling_many_files_stays_within_the_entry_budget() {
    let mut cache = NotificationCache::new();
    let quiet = ServerId::from("quiet");
    cache.set_diagnostics_route_count(2);
    for n in 0..10 {
        cache.store_diagnostics(
            &quiet,
            &Uri::from(format!("file:///quiet/{n}.rs")),
            None,
            vec![error(1, "q")],
        );
    }

    let mut reported = 0;
    for n in 0..1100 {
        let target = Uri::from(format!("file:///ws/pulled{n}.rs"));
        if let PullWrite::Stored { evicted, .. } =
            pull_for(&mut cache, &target, 1, vec![error(1, "p")])
        {
            reported += evicted.len();
        }
    }

    assert!(cache.diagnostics_count() <= MAX_DIAGNOSTIC_ENTRIES);
    assert_eq!(reported, 1100 + 10 - MAX_DIAGNOSTIC_ENTRIES);
    for n in 0..10 {
        assert!(
            cache
                .diagnostics(&Uri::from(format!("file:///quiet/{n}.rs")))
                .is_some()
        );
    }
    cache.assert_consistent();
}

#[test]
fn test_a_pull_at_capacity_never_evicts_its_own_files_pushed_slot() {
    let mut cache = NotificationCache::new();
    push(&mut cache, Some(1), vec![error(1, "flycheck")]);
    for n in 1..MAX_DIAGNOSTIC_ENTRIES {
        cache.store_diagnostics(
            &server(),
            &Uri::from(format!("file:///ws/filler{n}.rs")),
            None,
            vec![error(1, "f")],
        );
    }
    assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

    let PullWrite::Stored { evicted, .. } = pull(&mut cache, vec![error(2, "pulled")]) else {
        panic!("pull was discarded");
    };

    assert_eq!(
        evicted,
        [DiagnosticsKey::of(&Uri::from("file:///ws/filler1.rs"))]
    );
    assert!(cache.diagnostics(&file()).is_some());
    assert_eq!(messages(&cache), ["flycheck", "pulled"]);
    cache.assert_consistent();
}

#[test]
fn test_an_over_budget_pull_reports_the_evicted_files() {
    let mut cache = full_of_pushed_fillers();

    let PullWrite::Stored { evicted, .. } = pull(&mut cache, vec![error(1, "e")]) else {
        panic!("pull was discarded");
    };

    assert_eq!(evicted.len(), 1);
    assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
}

fn full_of_pushed_fillers() -> NotificationCache {
    let mut cache = NotificationCache::new();
    for n in 0..MAX_DIAGNOSTIC_ENTRIES {
        cache.store_diagnostics(
            &server(),
            &Uri::from(format!("file:///ws/filler{n}.rs")),
            None,
            vec![error(1, "f")],
        );
    }
    cache
}

#[test]
fn test_pulled_messages_are_truncated_like_pushed_ones() {
    let mut cache = NotificationCache::new();
    let huge = "x".repeat(MAX_ENTRY_TEXT_BYTES + 100);

    drop(pull(&mut cache, vec![error(1, &huge)]));

    let info = merged(&cache).unwrap();
    assert!(message_as_str(&info.diagnostics[0].message).len() < huge.len());
}

#[cfg(windows)]
#[test]
fn test_keys_differing_in_drive_letter_case_name_one_slot() {
    let mut cache = NotificationCache::new();
    let upper = Uri::from("file:///C:/ws/main.rs");
    let lower = Uri::from("file:///c:/ws/main.rs");
    drop(pull_for(&mut cache, &upper, 1, vec![error(1, "e")]));

    let write = pull_for(&mut cache, &lower, 1, vec![error(1, "e")]);

    assert!(matches!(
        write,
        PullWrite::Stored {
            slot: SlotChange::Identical,
            ..
        }
    ));
    assert_eq!(cache.diagnostics_count(), 1);
}

#[test]
fn test_change_outcome_distinguishes_absent_from_empty() {
    let empty = DiagnosticInfo {
        uri: file(),
        version: None,
        diagnostics: vec![],
    };

    assert_eq!(ChangeOutcome::of(None, None), ChangeOutcome::Unchanged);
    assert_eq!(
        ChangeOutcome::of(None, Some(&empty)),
        ChangeOutcome::Changed
    );
    assert_eq!(
        ChangeOutcome::of(Some(&empty), None),
        ChangeOutcome::Changed
    );
    assert_eq!(
        ChangeOutcome::of(Some(&empty), Some(&empty)),
        ChangeOutcome::Unchanged
    );
}

/// The pairwise rule `PulledIndex` replaces, kept as the oracle.
fn naive_same_problem(pulled: &LspDiagnostic, pushed: &LspDiagnostic) -> bool {
    fn position_le(a: Position, b: Position) -> bool {
        (a.line, a.character) <= (b.line, b.character)
    }

    let (pulled_code, pushed_code) = (reported_code(pulled), reported_code(pushed));
    let same_severity = ReportedSeverity::of(pulled) == ReportedSeverity::of(pushed);
    match (&pulled_code, &pushed_code) {
        (Some(a), Some(b)) if a == b && same_severity => {
            let (p, q) = (pulled.range, pushed.range);
            let overlaps = position_le(p.start, q.end) && position_le(q.start, p.end);
            overlaps || p.start.line.abs_diff(q.start.line) <= DUPLICATE_RANGE_PROXIMITY_LINES
        }
        _ => {
            pulled.range == pushed.range
                && same_severity
                && pulled_code == pushed_code
                && message_as_str(&pulled.message) == message_as_str(&pushed.message)
        }
    }
}

fn arbitrary_diagnostic() -> impl proptest::strategy::Strategy<Value = LspDiagnostic> {
    use proptest::prelude::*;

    let severity = prop_oneof![
        Just(None),
        Just(Some(DiagnosticSeverity::Error)),
        Just(Some(DiagnosticSeverity::Warning)),
        Just(Some(DiagnosticSeverity::Information)),
        Just(Some(DiagnosticSeverity::Hint)),
    ];
    let code = prop_oneof![
        Just(None),
        Just(Some(Code::Int(1))),
        Just(Some(Code::String("1".to_owned()))),
        Just(Some(Code::Int(2))),
        Just(Some(Code::String("E2".to_owned()))),
    ];
    (
        (0..14u32, 0..3u32),
        (-3..10i32, 0..3u32),
        severity,
        code,
        prop_oneof![Just("a"), Just("b")],
    )
        .prop_map(
            |((line, character), (end_offset, end_character), severity, code, message)| {
                LspDiagnostic {
                    range: Range {
                        start: Position { line, character },
                        end: Position {
                            line: line.saturating_add_signed(end_offset),
                            character: end_character,
                        },
                    },
                    severity,
                    code,
                    message: message.to_owned().into(),
                    ..LspDiagnostic::default()
                }
            },
        )
}

proptest::proptest! {
    #[test]
    fn pulled_index_agrees_with_the_pairwise_rule(
        pulled in proptest::collection::vec(arbitrary_diagnostic(), 0..8),
        pushed in proptest::collection::vec(arbitrary_diagnostic(), 0..8),
    ) {
        let index = PulledIndex::new(&pulled);
        for candidate in &pushed {
            let expected = pulled.iter().any(|p| naive_same_problem(p, candidate));
            proptest::prop_assert_eq!(index.contains_same_problem(candidate), expected);
        }
    }
}

/// A file at the entry cap with integer codes, nothing duplicated: the shape
/// that made every read cost seconds of worker CPU when each pushed item was
/// compared with every pulled one. Generous bound: the grouped lookup takes a
/// fraction of it, the pairwise scan several times over.
#[test]
fn test_merge_at_the_entry_cap_does_not_compare_every_pair() {
    let mut cache = NotificationCache::new();
    let item = |line: u32| {
        let mut d = diag(line, DiagnosticSeverity::Error, Some(Code::Int(308)), "e");
        d.message = format!("e{line}").into();
        d
    };
    let pulled: Vec<_> = (0..8500).map(|n| item(n * 10)).collect();
    drop(pull(&mut cache, pulled));
    for source in ["main.rs", "a1.rs", "a2.rs", "a3.rs"] {
        let pushed: Vec<_> = (0..2250).map(|n| item(n * 10 + 5)).collect();
        cache.store_published_diagnostics(&server(), &pushed_via(source), None, pushed);
    }

    let started = std::time::Instant::now();
    let info = merged(&cache).unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "merge took {:?}",
        started.elapsed()
    );
    assert!(!info.diagnostics.is_empty());
}

#[test]
fn test_unserializable_diagnostics_read_as_a_change() {
    let info = DiagnosticInfo {
        uri: file(),
        version: None,
        diagnostics: vec![error(1, "a")],
    };

    assert_eq!(
        ChangeOutcome::compare(Some(&info), Some(&info), |d| serde_json::to_vec(d).ok()),
        ChangeOutcome::Unchanged
    );
    assert_eq!(
        ChangeOutcome::compare(Some(&info), Some(&info), |_| None),
        ChangeOutcome::Changed
    );
}

#[test]
fn test_change_outcome_ignores_the_order_of_equal_ranges() {
    let info = |items: Vec<LspDiagnostic>| DiagnosticInfo {
        uri: file(),
        version: None,
        diagnostics: items,
    };
    let (a, b) = (error(1, "a"), error(1, "b"));

    let before = info(vec![a.clone(), b.clone()]);
    let after = info(vec![b, a]);

    assert_eq!(
        ChangeOutcome::of(Some(&before), Some(&after)),
        ChangeOutcome::Unchanged
    );
}

#[test]
fn test_a_stored_pulled_slot_is_bounded_like_a_pushed_one() {
    let mut cache = NotificationCache::new();
    let items: Vec<_> = (0..5000)
        .map(|n| error(n, &format!("diagnostic {n}: {}", "x".repeat(250))))
        .collect();
    assert!(serde_json::to_vec(&items).unwrap().len() > MAX_DIAGNOSTICS_ENTRY_BYTES);

    drop(pull(&mut cache, items));

    let slot = &cache.entries[&SlotKey::pulled(DiagnosticsKey::of(&file()))];
    let bytes = serde_json::to_vec(&slot.info.diagnostics).unwrap().len();
    assert!(bytes <= MAX_DIAGNOSTICS_ENTRY_BYTES, "{bytes} bytes");
    assert!(!slot.info.diagnostics.is_empty());
}

#[test]
fn test_a_lone_pulled_slot_is_sorted_and_capped_by_merge() {
    let unsorted = cache_overlay(vec![error(5, "late"), error(1, "early")]);
    let merged_unsorted = unsorted.merge().unwrap();
    let lines: Vec<_> = merged_unsorted
        .diagnostics
        .iter()
        .map(|d| d.range.start.line)
        .collect();
    assert_eq!(lines, [1, 5]);

    let oversized = (0..5000)
        .map(|n| error(n, &format!("diagnostic {n}: {}", "x".repeat(250))))
        .collect();
    let merged_oversized = cache_overlay(oversized).merge().unwrap();
    let bytes = serde_json::to_vec(&merged_oversized.diagnostics)
        .unwrap()
        .len();
    assert!(bytes <= MAX_DIAGNOSTICS_ENTRY_BYTES, "{bytes} bytes");
}

/// A snapshot whose only source is `items` as an unbounded pulled slot, which
/// the store would never produce.
fn cache_overlay(items: Vec<LspDiagnostic>) -> DiagnosticSources {
    NotificationCache::new()
        .diagnostic_sources(&file())
        .with_pulled(&file(), Some(1), BoundedDiagnostics(items))
}

/// #670: when a push supersedes a pulled slot, by the document's synced
/// version as the tracker reports it.
#[test]
fn test_pull_supersession_rule() {
    use DocumentSync::{NotOpen, Synced, Unattached};

    let cases = [
        (4, Some(5), Synced(4), true),
        (4, Some(5), Unattached, true),
        (4, Some(4), Synced(4), false),
        (4, Some(2), Synced(4), false),
        (4, Some(2), Synced(1), true),
        (4, Some(4), NotOpen, true),
        (4, None, Synced(4), false),
        (4, None, Synced(5), true),
        (4, None, Synced(1), true),
        (4, None, NotOpen, true),
        (4, Some(4), Unattached, false),
        (4, Some(2), Unattached, false),
        (4, None, Unattached, false),
    ];
    for (pulled, pushed, sync, superseded) in cases {
        assert_eq!(
            pull_is_superseded(pulled, pushed, sync),
            superseded,
            "pulled {pulled}, pushed {pushed:?}, {sync:?}"
        );
    }
}

/// A file on disk, tracked by a tracker the cache is attached to.
struct TrackedFile {
    _dir: tempfile::TempDir,
    tracker: Arc<DocumentTracker>,
    path: std::path::PathBuf,
    uri: Uri,
}

impl TrackedFile {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dunce::canonicalize(dir.path()).unwrap().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let uri = crate::bridge::path_to_uri(&path).unwrap();
        Self {
            _dir: dir,
            tracker: Arc::new(DocumentTracker::new(
                crate::bridge::ResourceLimits::default(),
                std::collections::HashMap::new(),
            )),
            path,
            uri,
        }
    }

    fn open_at(&self, version: i32) {
        self.tracker
            .open(self.path.clone(), "fn main() {}".to_owned())
            .unwrap();
        self.set_version(version);
    }

    fn set_version(&self, version: i32) {
        self.tracker
            .set_synced_version_for_test(&self.path, &server(), version);
    }

    fn cache_with_pull_at(&self, version: i32) -> NotificationCache {
        let mut cache = NotificationCache::new();
        cache.attach_documents(Arc::clone(&self.tracker));
        drop(pull_for(
            &mut cache,
            &self.uri,
            version,
            vec![error(1, "pulled")],
        ));
        cache
    }

    fn push(&self, cache: &mut NotificationCache, version: Option<i32>) {
        drop(cache.write_published_diagnostics(
            &server(),
            &PublishedDiagnosticsUri::for_test(self.uri.clone(), self.uri.clone()),
            version,
            vec![error(2, "pushed")],
        ));
    }

    fn shown(&self, cache: &NotificationCache) -> Vec<String> {
        cache
            .diagnostic_sources(&self.uri)
            .merge()
            .unwrap()
            .diagnostics
            .iter()
            .map(|d| message_as_str(&d.message).to_owned())
            .collect()
    }
}

#[test]
fn test_versionless_push_keeps_the_pull_while_the_document_is_at_its_version() {
    let file = TrackedFile::new();
    file.open_at(4);
    let mut cache = file.cache_with_pull_at(4);

    file.push(&mut cache, None);

    assert_eq!(file.shown(&cache), ["pulled", "pushed"]);
}

#[test]
fn test_versionless_push_drops_the_pull_once_the_document_moved_on() {
    let file = TrackedFile::new();
    file.open_at(4);
    let mut cache = file.cache_with_pull_at(4);
    file.set_version(5);

    file.push(&mut cache, None);

    assert_eq!(file.shown(&cache), ["pushed"]);
    cache.assert_consistent();
}

/// A document reopened after an LRU eviction restarts at a lower version.
#[test]
fn test_lower_versioned_push_drops_the_pull_of_a_reopened_document() {
    let file = TrackedFile::new();
    file.open_at(4);
    let mut cache = file.cache_with_pull_at(4);
    file.set_version(1);

    file.push(&mut cache, Some(1));

    assert_eq!(file.shown(&cache), ["pushed"]);
}

#[test]
fn test_push_drops_the_pull_of_a_document_that_is_not_open() {
    let file = TrackedFile::new();
    let mut cache = file.cache_with_pull_at(4);

    file.push(&mut cache, None);

    assert_eq!(file.shown(&cache), ["pushed"]);
}

#[test]
fn test_without_a_tracker_only_a_newer_version_supersedes_the_pull() {
    let file = TrackedFile::new();
    let mut cache = NotificationCache::new();
    drop(pull_for(&mut cache, &file.uri, 4, vec![error(1, "pulled")]));

    file.push(&mut cache, None);
    assert_eq!(file.shown(&cache), ["pulled", "pushed"]);
    file.push(&mut cache, Some(5));
    assert_eq!(file.shown(&cache), ["pushed"]);
}
