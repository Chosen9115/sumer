//! The mutation battery: the check that every assertion in this suite can
//! still be **made to fail**.
//!
//! # Why this exists
//!
//! Seven assertions in this suite were satisfied by a deliberately broken
//! implementation. Every one of them was found by breaking the adapter and
//! watching the suite stay green -- none by reading the code. This file is
//! that habit turned into an artifact: a directory of *mutants*, each one a
//! small, surgical break, each one required to be caught by a named set of
//! assertions.
//!
//! # The mechanism
//!
//! A mutant is `conformance/mutations/<name>.json`:
//!
//! ```json
//! { "fixture": "pending_to_posted",
//!   "assertions": ["A2"],
//!   "covers": ["A8"],
//!   "adapter": null,
//!   "why": "...",
//!   "patch": [{"path": "/script/runs/0/.../amount/amount", "value": "42.38"}] }
//! ```
//!
//! * `patch` is applied to a **copy** of the fixture, and may only touch
//!   `/script/...`. The fake adapter is a script interpreter, so a patched
//!   script *is* a broken adapter by construction -- this is real mutation,
//!   not a simulation of one. Patching `expect` would be the opposite:
//!   weakening the test instead of breaking the thing under test, which is
//!   rejected here rather than trusted to review.
//! * The case then runs against its **original, unmutated `expect`**.
//! * `adapter` names a small monkey-patch wrapper under
//!   `mutations/adapters/`, for the mutations a static patch cannot express:
//!   behaving differently on the *second* process invocation, or corrupting
//!   the adapter's degrade step rather than its data.
//! * `covers` is the `(fixture, assertion)` pair this mutant claims in
//!   [`COVERAGE`], when that differs from the assertion ids the failure
//!   messages actually carry. It is not free text; it is bound two
//!   different ways, because two different things are being claimed:
//!   * **A violation KIND is bound to the mechanism, not the label.**
//!     All nine fatal kinds file under `A11`, so the label proves only
//!     that *something* fatal happened. A mutant claiming
//!     `StdinEofIgnored` must produce a failure whose `mechanism` is
//!     `StdinEofIgnored` -- the kind the run actually reported, or the
//!     kind the fixture expected and did not get **on a run that reached
//!     the stimulus that would have produced it**. Claiming one kind while
//!     provoking another (garbage on the wire, say) no longer type-checks
//!     as coverage, and neither does a run that aborted on a prerequisite:
//!     pre-hello garbage on the run that tests oversized frames used to
//!     die at spawn and still report `OversizeFrame`, for a frame it never
//!     asked for. See `runner::prerequisite_failure`;
//!     `protocol_violations__oversize_run_dies_before_the_stimulus` is the
//!     mutant that holds it.
//!   * **An id with no failure label of its own** reaches one through
//!     [`COVERS_VIA`]: A8 (full history retention) is enforced inside A2's
//!     sequence equality, A3 (unknown is never zero) inside A1's balances
//!     comparison, and A10's content half inside A2's exact
//!     `provider_extra` comparison. These three are label-level only --
//!     see "the limits" below for what that does not establish.
//!
//!   Defaults to `assertions`.
//!
//! # The three rules that keep this from going hollow
//!
//! **1. EXACTNESS.** The set of distinct assertion ids in the resulting
//! failures must EQUAL `assertions`. A mutant that provokes an *unlisted*
//! id is too blunt and fails here; a mutant that provokes *none* is a
//! survivor and fails here. Without exactness, eleven blunt mutants (delete
//! a resource, kill the adapter) would satisfy a coverage count while
//! proving nothing about any single assertion's discriminating power -- and
//! the seven real hollows died to surgical mutations: bytes leaked beside a
//! truncation marker, the same ids carrying different money. A set of two
//! ids is fine and expected where one break genuinely violates two things
//! (an oversized leak is both A10 and the ledger's exact comparison);
//! exactness means *equal*, not *singleton*.
//!
//! **2. BINDING IS PER `(fixture, assertion)`, NEVER A BARE ID.** [`COVERAGE`]
//! is a table of pairs. Otherwise `pending_to_posted` could quietly stop
//! asserting A8 while `reorg_vanish`'s A8 mutant kept that id green -- which
//! is exactly the drift this battery exists to stop.
//!
//! **3. ONLY A MUTANT THAT KILLS MAY BACK A PAIR.** An EQUIVALENT mutant is
//! *required to produce no failure at all*; a coverage claim resting on one
//! is a claim that nothing can fail, which is the hollow assertion this
//! whole battery exists to prevent -- here, inside the tool built to
//! prevent it. `covers: ["A8"]` on an equivalent wrapper plus a deleted
//! real A8 mutant would have left A8 "backed" by something that cannot go
//! red. `the_coverage_table_is_backed_by_mutants` therefore builds its
//! `backed` set from killing mutants only, and rule 2 above (`covers` must
//! reach an id the mutant actually provokes) makes the same claim
//! impossible to write down in the first place.
//!
//! # The limits this machinery does NOT cover -- review obligations
//!
//! **1. A widened manifest.** Exactness is checked against **the set a
//! manifest declares**. It catches a mutant that grows an unlisted id under
//! a fixed manifest; it cannot catch a manifest widened to match a blunter
//! mutant. Adding `"A2"` to a mutant's `assertions` because it "also trips
//! A2" is indistinguishable, mechanically, from a mutant that legitimately
//! violates two things. No machinery here can tell the two apart: the
//! difference is whether the second id names a *distinct* break or the
//! collateral damage of a blunt one.
//!
//! So: **widening a declared set is a review decision, not a fix.** If a
//! mutant stops matching its manifest, the first question is whether the
//! mutation got blunter, and the manifest's `why` must say which -- see the
//! CONSTRAINTS in `CLAUDE.md`. This is written down rather than pretended
//! away.
//!
//! **2. Substitution inside one label, for the three [`COVERS_VIA`] rows.**
//! `A8`, `A3` and `A10` are bound to a LABEL (`A2`, `A1`, `A2`), not to a
//! mechanism: nothing here checks that the A2 failure a mutant claiming A8
//! provoked was about history retention rather than, say, a drifted amount
//! in the same sequence comparison. Both are A2. The violation kinds used
//! to have this same hole and no longer do (they are bound to
//! `Failure::mechanism`); these three still do, because the assertions
//! behind them produce one undifferentiated diff and inventing a mechanism
//! label per diff shape would be machinery inventing a distinction the
//! assertion itself does not make. What holds them is the `why` field and
//! review, and that is stated here rather than implied to be checked.
//!
//! **3. Nothing binds a mutant to the SPECIFIC field it broke.** Exactness
//! and the mechanism binding both work on sets of ids; a mutant that
//! patches a different pointer than its `why` describes, and trips the same
//! ids, reads as identical. The patch list is in the manifest, next to the
//! prose, for exactly this reason.
//!
//! This is not a hypothetical: it is what let a coverage claim be paid for
//! by a *prerequisite* failure. A mutant may patch any `/script/...`
//! pointer of the fixture, including one belonging to a different run's
//! handshake, and nothing here notices that the break it performed is not
//! the break its claim is about. The mechanism binding closes that for the
//! violation kinds (a prerequisite abort now reports what actually
//! happened, never the kind the fixture wanted -- see
//! `runner::prerequisite_failure`); for the assertion ids it remains a
//! review obligation, held by `why` and by the patch list beside it.
//!
//! **4. A stimulus dispatched onto an already-dead connection still credits
//! the kind its fixture expected.** `runner::prerequisite_failure` covers a
//! run that short-circuits BEFORE its stimulus. It does not cover a frame
//! that was written to a connection which had already died -- run 1's final
//! `status.read` answering `AdapterCrashed`, and the `wait_for_violation`
//! and `balances.read` arms beside it. The frame went out; whether the
//! adapter ever read it is unestablished, so "the violation stopped
//! happening" and "the adapter never saw the request" are indistinguishable
//! there. Narrower than limit 3 -- reaching it needs a mutant that kills the
//! connection mid-run and trips nothing else -- but it is the same shape,
//! and it is written here rather than left to be discovered.
//!
//! # Live and parked
//!
//! A mutant whose fixture already fails unmutated cannot demonstrate
//! anything: the fixture's own noise would answer for it. So this harness
//! **measures** rather than keeping a list: it runs each fixture unmutated
//! first, and a fixture whose baseline is not clean has its mutants PARKED,
//! reported and counted rather than silently skipped. Every fixture's
//! baseline is clean today and nothing is parked; the path stays because a
//! regression on `main` must show up as "these mutants can no longer prove
//! anything", not as a battery that quietly agrees with itself. A parked
//! mutant is still run, its patch must still apply, and whatever it provokes
//! on top of the baseline must still be a subset of what it names.
//!
//! `cargo test -p sumer-conformance --test mutations -- --nocapture` prints
//! the whole ledger: what was killed, what is parked and behind what.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The eleven named assertions of the frozen contract's section (g).
const ASSERTIONS: &[&str] = &[
    "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "A9", "A10", "A11",
];

/// The fatal protocol-violation kinds, which are constraints in their own
/// right (a host that reports `NotJson` for a non-UTF-8 frame has not
/// detected the violation, it has guessed). Every one of them is reported
/// under the `A11` label, so a claim naming one is bound to the failure's
/// `mechanism` instead -- see the module docs.
const VIOLATION_KINDS: &[&str] = &[
    "PreHelloOutput",
    "UnknownId",
    "DuplicateId",
    "OversizeFrame",
    "NotJson",
    "NonUtf8",
    "UnterminatedFrame",
    "StdinEofIgnored",
    "StdoutHeldOpen",
];

/// Every wire constraint a mutant can be bound to.
fn constraints() -> impl Iterator<Item = &'static str> {
    ASSERTIONS.iter().chain(VIOLATION_KINDS).copied()
}

/// The complete list of LABEL indirections a mutant's `covers` may use: an
/// id it claims, paired with the id its own failure messages carry instead.
/// Three rows, and each one is a fact about a constraint that has no
/// failure label of its own:
///
/// * `A8` (full history retention, in order) -- enforced by A2's sequence
///   equality;
/// * `A3` (unknown is never zero) -- enforced by the balances sequence
///   comparison, which files under `A1`;
/// * `A10`'s *content* half (`provider_extra` is exactly the truncation
///   marker and nothing beside it) -- enforced by the ledger's exact
///   comparison, which files under `A2`. A10's *size* half does file under
///   `A10`, which is why an over-cap leak produces both.
///
/// **The violation kinds are deliberately NOT in this table.** They used to
/// be, as seven rows pointing at `A11`, and that was a licence rather than
/// a fact: any mutant that provoked any fatal violation could claim any
/// kind, because the label they all share was the only thing checked. They
/// are bound to the failure's `mechanism` instead -- the kind the run
/// reported, or the kind the fixture expected and did not get -- which is
/// the thing the claim is actually about.
///
/// A row here says "this constraint is enforced under another id's label",
/// and what it does NOT say is that the specific failure was about this
/// constraint: see limit 2 in the module docs. Adding a row is a review
/// decision, to be shown rather than assumed.
const COVERS_VIA: &[(&str, &str)] = &[("A8", "A2"), ("A3", "A1"), ("A10", "A2")];

/// Which constraints each fixture is claimed to discriminate. **The unit is
/// the pair**: `("pending_to_posted", "A8")` and `("reorg_vanish", "A8")`
/// are two independent claims, and each needs its own mutant. A bare list of
/// ids could be kept green by one fixture while every other one quietly
/// stopped asserting anything.
const COVERAGE: &[(&str, &[&str])] = &[
    ("duplicate_events", &["A2", "A9"]),
    ("fdx_lossless", &["A1"]),
    ("interrupted_pagination", &["A1", "A5", "A10"]),
    ("large_amounts", &["A1", "A2"]),
    ("null_category", &["A3"]),
    ("oversized_observation", &["A10"]),
    (
        "pending_to_posted",
        &[
            "A1",
            "A2",
            "A6",
            "A7",
            "A8",
            "A9",
            "A10",
            "A11",
            "NotJson",
            "UnterminatedFrame",
            "StdinEofIgnored",
            "StdoutHeldOpen",
        ],
    ),
    (
        "protocol_violations",
        &[
            "A4",
            "A11",
            "PreHelloOutput",
            "UnknownId",
            "DuplicateId",
            "OversizeFrame",
            "NotJson",
            "NonUtf8",
        ],
    ),
    ("provider_json_number", &["A1", "A4"]),
    ("reorg_vanish", &["A2"]),
    ("stale_balance", &["A7"]),
    ("unsupported_op", &["A4"]),
];

/// One mutant, as it is written down. Unknown keys are rejected: a typo in
/// a manifest is a mutant that silently stops mutating.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutant {
    fixture: String,
    /// The exact set of assertion ids the resulting failures must carry.
    /// Empty means "this mutation must provoke nothing" -- see
    /// [`equivalent`](Mutant::equivalent).
    assertions: Vec<String>,
    /// The `(fixture, constraint)` pairs this mutant claims in [`COVERAGE`].
    /// Defaults to `assertions`.
    #[serde(default)]
    covers: Option<Vec<String>>,
    /// A monkey-patch wrapper under `mutations/adapters/`, for mutations a
    /// static patch cannot express.
    #[serde(default)]
    adapter: Option<String>,
    /// Why this break is worth having a mutant for. Required, and required
    /// to be non-empty: an empty `assertions` list is only ever legitimate
    /// when someone has written down why.
    why: String,
    patch: Vec<Patch>,
}

impl Mutant {
    /// A mutant that must provoke **nothing**: the break it performs is not
    /// observable through this suite's aperture, and it is kept as a guard
    /// -- if the aperture ever widens back, this stops being silent and the
    /// battery goes red.
    fn equivalent(&self) -> bool {
        self.assertions.is_empty()
    }

    fn claims(&self) -> Vec<String> {
        self.covers
            .clone()
            .unwrap_or_else(|| self.assertions.clone())
    }
}

/// One JSON-pointer assignment against the fixture's `script`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Patch {
    path: String,
    value: Value,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("conformance/ has a parent directory")
        .to_path_buf()
}

fn mutations_dir() -> PathBuf {
    repo_root().join("conformance/mutations")
}

/// Every manifest, by name (the file stem), sorted.
fn load_mutants() -> Vec<(String, Mutant)> {
    let dir = mutations_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("could not read {dir:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no mutants found under {dir:?}");
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .expect("a *.json path has a stem")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
            let mutant: Mutant = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{path:?} is not a valid mutant manifest: {e}"));
            (name, mutant)
        })
        .collect()
}

/// Applies one pointer assignment, and **fails loudly on anything that would
/// leave the fixture unmutated**: a path outside `/script`, a parent that
/// does not exist, or a value that was already there. A patch that quietly
/// does nothing is a survivor by construction, which is the one failure mode
/// this whole file exists to prevent.
fn apply(fixture: &mut Value, patch: &Patch) -> Result<(), String> {
    if !patch.path.starts_with("/script/") {
        return Err(format!(
            "{}: a mutant may only patch /script/... -- it breaks the ADAPTER, never the \
             expectations it is judged against",
            patch.path
        ));
    }
    if let Some(slot) = fixture.pointer_mut(&patch.path) {
        if *slot == patch.value {
            return Err(format!(
                "{}: the patched value is already what the fixture says -- this mutant mutates \
                 nothing",
                patch.path
            ));
        }
        *slot = patch.value.clone();
        return Ok(());
    }
    let (parent, last) = patch
        .path
        .rsplit_once('/')
        .ok_or_else(|| format!("{}: not a JSON pointer", patch.path))?;
    let key = last.replace("~1", "/").replace("~0", "~");
    match fixture.pointer_mut(parent) {
        Some(Value::Object(map)) => {
            map.insert(key, patch.value.clone());
            Ok(())
        }
        _ => Err(format!(
            "{}: neither this path nor an object at {parent:?} exists -- the fixture moved and \
             this mutant is now a no-op",
            patch.path
        )),
    }
}

/// The mutated fixture, written to a scratch file, plus the argv that drives
/// it. Returns an error rather than panicking so a stale manifest is
/// reported beside every other failure instead of aborting the run.
fn materialize(root: &Path, name: &str, mutant: &Mutant) -> Result<(PathBuf, Vec<String>), String> {
    let case_path = root
        .join("conformance/cases")
        .join(format!("{}.json", mutant.fixture));
    let text = std::fs::read_to_string(&case_path)
        .map_err(|e| format!("could not read {case_path:?}: {e}"))?;
    let original: Value =
        serde_json::from_str(&text).map_err(|e| format!("{case_path:?} is not JSON: {e}"))?;
    let mut mutated = original.clone();
    for patch in &mutant.patch {
        apply(&mut mutated, patch)?;
    }
    if mutated == original && mutant.adapter.is_none() {
        return Err("the patch list left the fixture unchanged".to_owned());
    }

    let adapter_argv = match &mutant.adapter {
        None => vec![
            "python3".to_owned(),
            root.join("adapters/fake/fake_adapter.py")
                .to_string_lossy()
                .into_owned(),
        ],
        Some(wrapper) => {
            let path = mutations_dir().join("adapters").join(wrapper);
            if !path.is_file() {
                return Err(format!("adapter wrapper {path:?} does not exist"));
            }
            vec!["python3".to_owned(), path.to_string_lossy().into_owned()]
        }
    };

    let dir = std::env::temp_dir().join("sumer-mutations");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {dir:?}: {e}"))?;
    let path = dir.join(format!("{name}.json"));
    // A wrapper counts process invocations in a file beside its fixture;
    // last run's count must not leak into this one.
    let _ = std::fs::remove_file(dir.join(format!("{name}.json.invocations")));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&mutated).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("could not write {path:?}: {e}"))?;
    Ok((path, adapter_argv))
}

/// The distinct assertion ids one run produced.
fn ids(failures: &[sumer_conformance::assert::Failure]) -> BTreeSet<String> {
    failures.iter().map(|f| f.assertion.clone()).collect()
}

/// The distinct violation MECHANISMS one run produced -- the specific
/// `ProtocolViolationKind` each `A11` failure was established by or about.
/// This is what a `covers` claim naming a kind is checked against: `A11`
/// alone is the label nine different breaks share.
fn mechanisms(failures: &[sumer_conformance::assert::Failure]) -> BTreeSet<String> {
    failures
        .iter()
        .filter_map(|f| f.mechanism.clone())
        .collect()
}

fn render(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{{}}}", set.iter().cloned().collect::<Vec<_>>().join(", "))
    }
}

#[tokio::test]
async fn every_mutant_is_caught_by_exactly_the_assertions_it_names() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not found on PATH -- skipping the mutation battery");
        return;
    }
    let root = repo_root();
    let mutants = load_mutants();

    // Which fixtures are judgeable today. Measured, not listed: a fixture
    // that fails unmutated cannot demonstrate that a mutation was caught.
    let mut baseline: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for fixture in mutants
        .iter()
        .map(|(_, m)| m.fixture.clone())
        .collect::<BTreeSet<_>>()
    {
        let path = root
            .join("conformance/cases")
            .join(format!("{fixture}.json"));
        let argv = vec![
            "python3".to_owned(),
            root.join("adapters/fake/fake_adapter.py")
                .to_string_lossy()
                .into_owned(),
        ];
        let outcome = sumer_conformance::runner::run_case(&argv, &path).await;
        baseline.insert(fixture, ids(&outcome.failures));
    }

    let mut report = Vec::new();
    let mut failures = Vec::new();
    let (mut live, mut parked, mut equivalent) = (0_u32, 0_u32, 0_u32);

    for (name, mutant) in &mutants {
        let (path, argv) = match materialize(&root, name, mutant) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        let expected: BTreeSet<String> = mutant.assertions.iter().cloned().collect();
        let baseline = baseline.get(&mutant.fixture).cloned().unwrap_or_default();
        let outcome = sumer_conformance::runner::run_case(&argv, &path).await;
        let produced = ids(&outcome.failures);
        // A claim naming a violation KIND has to be backed by the kind the
        // execution actually reported (or expected and did not get), not
        // merely by the `A11` label every kind shares. Without this a
        // mutant provoking ordinary JSON garbage could claim
        // `StdinEofIgnored` and every check here would still pass.
        let produced_kinds = mechanisms(&outcome.failures);
        for id in mutant.claims() {
            if VIOLATION_KINDS.contains(&id.as_str()) && !produced_kinds.contains(&id) {
                failures.push(format!(
                    "{name}: claims coverage of the violation kind {id:?}, but this run \
                     established {} -- a kind claim is bound to the mechanism, not to the A11 \
                     label all of them share\n    mutant:   {}",
                    render(&produced_kinds),
                    path.display(),
                ));
            }
        }

        // A fixture that fails unmutated cannot demonstrate that a mutation
        // was caught: its own noise would answer for the mutant. What IS
        // checkable today is that the mutation adds nothing the manifest
        // does not name -- most of this suite's assertions run inside the
        // crawl and are not gated behind `expect.ledger`, so a parked
        // mutant that is already too blunt is caught now rather than in
        // whichever week its fixture gets converted.
        if !baseline.is_empty() {
            parked += 1;
            let added: BTreeSet<String> = produced.difference(&baseline).cloned().collect();
            report.push(format!(
                "PARKED {name}\n         fixture {:?} is not converted yet (it fails unmutated \
                 with {}), so nothing here can be killed until it is. Declared {}; on top of \
                 that baseline the mutation adds {} today.",
                mutant.fixture,
                render(&baseline),
                render(&expected),
                render(&added),
            ));
            if !added.is_subset(&expected) {
                failures.push(format!(
                    "{name}: TOO BLUNT ALREADY -- on top of its fixture's unmutated baseline {}, \
                     this mutation provokes {}, which is not a subset of the {} it names\n    \
                     mutant: {}",
                    render(&baseline),
                    render(&added),
                    render(&expected),
                    path.display(),
                ));
            }
            continue;
        }

        if produced == expected {
            if mutant.equivalent() {
                equivalent += 1;
                report.push(format!(
                    "EQUIV  {name}\n         provokes nothing, by design: {}",
                    mutant.why
                ));
            } else {
                live += 1;
                report.push(format!(
                    "KILLED {name}\n         declared {} == produced {}",
                    render(&expected),
                    render(&produced)
                ));
            }
            continue;
        }
        let verdict = if produced.is_empty() {
            "SURVIVED -- the mutation provoked no failure at all"
        } else if expected.is_empty() {
            "NOT EQUIVALENT -- this mutant was declared unobservable and was observed"
        } else if expected.is_subset(&produced) {
            "TOO BLUNT -- it provoked assertions it does not name"
        } else {
            "MISSED -- it did not provoke every assertion it names"
        };
        failures.push(format!(
            "{name}: {verdict}\n    fixture:  {}\n    declared: {}\n    produced: {}\n    why:  \
             {}\n    mutant:   {}",
            mutant.fixture,
            render(&expected),
            render(&produced),
            mutant.why,
            path.display(),
        ));
    }

    report.sort();
    eprintln!(
        "\nmutation battery: {live} killed, {equivalent} equivalent (declared unobservable), \
         {parked} parked behind an unconverted fixture, {} mutants total\n{}\n",
        mutants.len(),
        report.join("\n")
    );
    assert!(
        failures.is_empty(),
        "the mutation battery is not sound:\n\n{}\n",
        failures.join("\n\n")
    );
}

/// The coverage table itself, checked in every direction that can rot.
#[test]
fn the_coverage_table_is_backed_by_mutants() {
    let mutants = load_mutants();
    let claimed: BTreeSet<(&str, &str)> = COVERAGE
        .iter()
        .flat_map(|(fixture, ids)| ids.iter().map(move |id| (*fixture, *id)))
        .collect();
    let known: BTreeSet<&str> = constraints().collect();

    // 1. The table names only real constraints.
    for (fixture, id) in &claimed {
        assert!(
            known.contains(id),
            "COVERAGE claims {id:?} for {fixture:?}, which is not a wire constraint"
        );
    }

    // 2. Every constraint is claimed by at least one fixture. An unclaimed
    //    constraint is one nothing in this suite is known to discriminate.
    for constraint in constraints() {
        assert!(
            claimed.iter().any(|(_, id)| *id == constraint),
            "no fixture claims {constraint:?} -- nothing here proves any case can tell it apart"
        );
    }

    // 3. Every claimed PAIR has a mutant that KILLS, and every such
    //    mutant's claim is in the table. Pairs, not bare ids: see the note
    //    on COVERAGE.
    //
    //    **Equivalent mutants back nothing.** They are required to produce
    //    no failure at all, so a pair resting on one is backed by something
    //    that cannot go red -- a hollow assertion inside the battery built
    //    to catch hollow assertions. Their value is the reachability check
    //    below (`the_equivalent_mutants_could_still_fire`), never coverage.
    let backed: BTreeSet<(String, String)> = mutants
        .iter()
        .filter(|(_, m)| !m.equivalent())
        .flat_map(|(_, m)| {
            m.claims()
                .into_iter()
                .map(move |id| (m.fixture.clone(), id))
        })
        .collect();
    for (fixture, id) in &claimed {
        assert!(
            backed.contains(&((*fixture).to_owned(), (*id).to_owned())),
            "COVERAGE claims ({fixture:?}, {id:?}) but no mutant breaks it -- the claim is prose"
        );
    }
    for (fixture, id) in &backed {
        assert!(
            claimed.contains(&(fixture.as_str(), id.as_str())),
            "a mutant claims ({fixture:?}, {id:?}), which COVERAGE does not -- the table is \
             out of date"
        );
    }

    // 4. Every fixture in the suite has at least one mutant, and every
    //    mutant names a fixture that exists.
    let cases_dir = repo_root().join("conformance/cases");
    let fixtures: BTreeSet<String> = std::fs::read_dir(&cases_dir)
        .unwrap_or_else(|e| panic!("could not read {cases_dir:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| Some(p.file_stem()?.to_string_lossy().into_owned()))
        .collect();
    let mutated: BTreeSet<String> = mutants.iter().map(|(_, m)| m.fixture.clone()).collect();
    assert_eq!(
        mutated, fixtures,
        "every fixture needs at least one mutant, and every mutant needs a real fixture"
    );
    assert_eq!(
        COVERAGE
            .iter()
            .map(|(f, _)| (*f).to_owned())
            .collect::<BTreeSet<_>>(),
        fixtures,
        "COVERAGE must have exactly one row per fixture"
    );

    // 5. A mutant that declares nothing must say why in prose, and a mutant
    //    that declares something must still say why. `why` is the only field
    //    a reader has to tell a deliberate no-op from a forgotten one.
    for (name, mutant) in &mutants {
        assert!(
            mutant.why.len() > 20,
            "mutant {name:?} must explain what break it performs and why it matters"
        );
        assert!(
            !mutant.patch.is_empty() || mutant.adapter.is_some(),
            "mutant {name:?} breaks nothing: it neither patches the script nor names a wrapper"
        );
    }

    // 6. A `covers` id must be connected to what the mutant actually
    //    provokes: either an id its own failures carry, or one that reaches
    //    such an id through COVERS_VIA. Without this, `covers` is free text
    //    -- a mutant could claim any constraint in the table while breaking
    //    something entirely unrelated, and the pair would read as backed.
    //    It also makes rule 3 unwritable rather than merely unmet: an
    //    equivalent mutant provokes nothing, so nothing it could name would
    //    pass here.
    for (name, mutant) in &mutants {
        for id in mutant.claims() {
            let direct = mutant.assertions.contains(&id);
            let via = COVERS_VIA.iter().any(|(claimed, reported_as)| {
                *claimed == id && mutant.assertions.iter().any(|a| a == reported_as)
            });
            // A violation kind is filed under `A11`; WHICH kind is checked
            // against the failure's mechanism when the battery actually
            // runs it, since only the run knows what it provoked.
            let kind = VIOLATION_KINDS.contains(&id.as_str())
                && mutant.assertions.iter().any(|a| a == "A11");
            assert!(
                direct || via || kind,
                "mutant {name:?} claims coverage of {id:?}, but its failures carry {:?} and \
                 {id:?} is neither one of the indirections COVERS_VIA documents nor a violation \
                 kind reported under an A11 the mutant names. A claim unconnected to what the \
                 mutation provokes is prose",
                mutant.assertions,
            );
        }
    }
}

/// The two EQUIVALENT mutants (`empty_reads_without_discovery`,
/// `ids_prefixed_without_discovery`) are declared unobservable because this
/// suite no longer opens a connection whose reads begin without a
/// `resources.list` -- there is one recorded crawl and it always discovers
/// first. Their entire remaining value is that they go red the day a
/// distinguishable second pass comes back.
///
/// A guard that cannot fire is dead weight, so this test fires it: each
/// wrapper is driven directly over exactly such a connection and must
/// diverge from the honest adapter. If a refactor of `fake_adapter.py` ever
/// leaves a wrapper's monkey-patch inert, that shows up here rather than as
/// two mutants silently agreeing they see nothing.
#[tokio::test]
async fn the_equivalent_mutants_could_still_fire() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not found on PATH -- skipping the equivalence reachability check");
        return;
    }
    let root = repo_root();
    // A COPY, in the scratch dir. `_wrapper.py` counts process invocations
    // in a file beside `SUMER_FIXTURE`; pointed at the real fixture it
    // would leave that marker sitting in `conformance/cases/`.
    let dir = std::env::temp_dir().join("sumer-mutations");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("could not create {dir:?}: {e}"));
    let fixture = dir.join("equivalence_probe.json");
    std::fs::copy(
        root.join("conformance/cases/pending_to_posted.json"),
        &fixture,
    )
    .unwrap_or_else(|e| panic!("could not stage {fixture:?}: {e}"));
    let _ = std::fs::remove_file(dir.join("equivalence_probe.json.invocations"));

    // One connection, no `resources.list` before the read: the shape the
    // deleted "wire pass" had, and the only shape these wrappers react to.
    async fn undiscovered_read(adapter: &Path, fixture: &Path) -> Vec<String> {
        let handle = sumer_host::AdapterHandle::spawn(
            vec!["python3".to_owned(), adapter.to_string_lossy().into_owned()],
            [
                (
                    "SUMER_FIXTURE".to_owned(),
                    fixture.to_string_lossy().into_owned(),
                ),
                ("SUMER_FIXTURE_RUN".to_owned(), "0".to_owned()),
            ],
        )
        .await
        .unwrap_or_else(|e| panic!("could not spawn {adapter:?}: {e}"));
        handle
            .history_read(vec![sumer_wire::ResourceQuery {
                resource_id: "checking-1".to_owned(),
                page: None,
            }])
            .await
            .unwrap_or_else(|e| panic!("{adapter:?}: history.read failed: {e}"))
            .observations
            .into_iter()
            .map(|o| o.local_id)
            .collect()
    }

    let honest = undiscovered_read(&root.join("adapters/fake/fake_adapter.py"), &fixture).await;
    assert!(
        !honest.is_empty(),
        "the honest adapter must answer an undiscovered read with real data, else this check \
         proves nothing"
    );
    for wrapper in [
        "empty_without_discovery.py",
        "ids_prefixed_without_discovery.py",
    ] {
        let broken =
            undiscovered_read(&mutations_dir().join("adapters").join(wrapper), &fixture).await;
        assert_ne!(
            broken, honest,
            "{wrapper} no longer diverges from the honest adapter on a connection that skips \
             resources.list -- its mutant is declared EQUIVALENT and can no longer fire, so it \
             is guarding nothing"
        );
    }
}
