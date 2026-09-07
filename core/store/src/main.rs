//! `sumer`: the CLI.
//!
//! Seven commands. `init`, `connect` and `refresh` write and hold the
//! profile lock for their whole run; `balances`, `history`, `show` and
//! `status` read and do not, so a report still answers while a refresh is
//! running. (`export`/`import` are PR 5.)
//!
//! Exit status: `0` fine, `1` a resource failed, `2` bad usage or a broken
//! profile, `3` another writer holds the profile lock.

use std::collections::BTreeMap;
use std::process::ExitCode;

use sumer_host::AdapterHandle;
use sumer_store::error::StoreError;
use sumer_store::store::{self, Store};
use sumer_store::{refresh, render, sweep, Profile};

const USAGE: &str = "\
sumer -- a local-first ledger of what your providers say

USAGE
  sumer [--profile <dir>] <command> [options]

COMMANDS
  init                      create the profile (prints what is and is not
                            protected -- read it once)
  connect <argv>...         run an adapter, record it and its resources
  refresh [--resume]        sweep every connected adapter
          [--confirm-empty] retract 100% of a resource (the only way)
          [--adapter <id>]
  balances                  every balance line, with source and freshness
  history [--resource <id>] the live records
  show <local_id>           one record's whole chain, retractions included
  status                    adapters, resources, crawls and discrepancies

  --profile <dir>           default: $SUMER_PROFILE, else ~/.sumer

EXIT
  0 ok · 1 a resource failed · 2 usage or profile error · 3 lock held";

/// `println!`, minus the panic when the reader goes away.
///
/// `sumer history | head` closes the pipe after ten lines, and the standard
/// `println!` treats that write error as a panic -- so the first thing
/// anyone does with a long list ends in a backtrace and a non-zero status.
/// A closed stdout is not a failure of this program: it is the ordinary end
/// of output, and every well-behaved Unix tool stops quietly. Exit 0 and
/// say nothing.
///
/// It writes through a locked handle and checks the result rather than
/// masking `SIGPIPE`, which would need `unsafe` in a crate that forbids it.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if writeln!(handle, $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(StoreError::ProfileLocked(path)) => {
            eprintln!(
                "sumer: another sumer process is writing to the profile at {}.\n\
                 Two writers would each derive retractions from a live set the other \
                 is changing underneath it; one refresh's older crawl would re-activate \
                 what the other correctly retracted. Wait for it, or use --profile.",
                path.display()
            );
            ExitCode::from(3)
        }
        Err(StoreError::Usage(message)) => {
            eprintln!("sumer: {message}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("sumer: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> sumer_store::Result<ExitCode> {
    let (profile_dir, rest) = split_profile(args)?;
    let profile = Profile::new(profile_dir);
    let Some((command, options)) = rest.split_first() else {
        out!("{USAGE}");
        return Ok(ExitCode::from(2));
    };

    match command.as_str() {
        "init" => cmd_init(&profile),
        "connect" => cmd_connect(&profile, options),
        "refresh" => cmd_refresh(&profile, options),
        "balances" => cmd_balances(&profile),
        "history" => cmd_history(&profile, options),
        "show" => cmd_show(&profile, options),
        "status" => cmd_status(&profile),
        "help" | "--help" | "-h" => {
            out!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(StoreError::Usage(format!(
            "unknown command {other:?}. Try `sumer help`."
        ))),
    }
}

/// `--profile <dir>` must be parsed before the command, because `connect`
/// takes a raw adapter argv that this parser must not touch.
fn split_profile(args: &[String]) -> sumer_store::Result<(std::path::PathBuf, Vec<String>)> {
    let mut rest = Vec::new();
    let mut dir: Option<std::path::PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--profile" {
            let value = iter
                .next()
                .ok_or_else(|| StoreError::Usage("--profile needs a directory".to_owned()))?;
            dir = Some(std::path::PathBuf::from(value));
        } else {
            rest.push(arg.clone());
            rest.extend(iter.cloned());
            break;
        }
    }
    let dir = dir
        .or_else(|| std::env::var_os("SUMER_PROFILE").map(std::path::PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".sumer")))
        .ok_or_else(|| {
            StoreError::Usage("no profile: set --profile or SUMER_PROFILE".to_owned())
        })?;
    Ok((dir, rest))
}

fn cmd_init(profile: &Profile) -> sumer_store::Result<ExitCode> {
    let _lock = profile.lock()?;
    Store::init(profile)?;
    out!("{}", sumer_store::profile::INIT_NOTICE);
    out!("\n  Profile: {}", profile.dir().display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_connect(profile: &Profile, argv: &[String]) -> sumer_store::Result<ExitCode> {
    if argv.is_empty() {
        return Err(StoreError::Usage(
            "connect needs an adapter command line, e.g. `sumer connect ./adapter --wallets w.json`"
                .to_owned(),
        ));
    }
    let _lock = profile.lock()?;
    let store = Store::open(profile)?;
    let argv = argv.to_vec();
    let (adapter_id, resources) = block_on(async {
        let handle = AdapterHandle::spawn(argv.clone(), []).await?;
        let adapter_id = handle.hello().adapter_id.clone();
        let derivation = handle.hello().local_id_derivation.clone();
        let listed = handle.resources_list().await?;
        let _ = handle.close().await;
        Ok::<_, StoreError>(((adapter_id, derivation), listed.resources))
    })?;
    let (adapter_id, derivation) = adapter_id;

    store::upsert_adapter(store.conn(), &adapter_id, &argv)?;
    store::set_adapter_derivation(store.conn(), &adapter_id, &derivation)?;
    for descriptor in &resources {
        store::upsert_resource(
            store.conn(),
            &adapter_id,
            &descriptor.resource_id,
            &descriptor.kind,
            &descriptor.label,
        )?;
    }
    out!("connected {adapter_id} ({derivation})");
    for descriptor in &resources {
        out!(
            "  {}  {}  [{}]",
            descriptor.resource_id,
            descriptor.label,
            descriptor.kind
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_refresh(profile: &Profile, options: &[String]) -> sumer_store::Result<ExitCode> {
    let mut refresh_options = refresh::RefreshOptions::default();
    let mut iter = options.iter();
    while let Some(option) = iter.next() {
        match option.as_str() {
            "--resume" => refresh_options.resume = true,
            "--confirm-empty" => refresh_options.confirm_empty = true,
            "--adapter" => {
                refresh_options.adapter_id = Some(
                    iter.next()
                        .ok_or_else(|| StoreError::Usage("--adapter needs an id".to_owned()))?
                        .clone(),
                );
            }
            other => {
                return Err(StoreError::Usage(format!(
                    "refresh: unknown option {other:?}"
                )))
            }
        }
    }

    let _lock = profile.lock()?;
    let mut store = Store::open(profile)?;
    let report = block_on(refresh::refresh(&mut store, &refresh_options))?;

    for (adapter_id, error) in &report.adapter_errors {
        out!("{adapter_id}: {error}");
    }
    for sweep_report in &report.sweeps {
        out!("{}", render::sweep_line(sweep_report));
        for discrepancy in &sweep_report.discrepancies {
            out!("  ! {}: {}", discrepancy.kind, discrepancy.detail);
        }
    }
    Ok(if report.failed() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_balances(profile: &Profile) -> sumer_store::Result<ExitCode> {
    let store = Store::open(profile)?;
    for resource in store::resources(store.conn(), None)? {
        out!(
            "{}/{}  {}",
            resource.adapter_id,
            resource.resource_id,
            resource.label
        );
        let history =
            store::balance_history(store.conn(), &resource.adapter_id, &resource.resource_id)?;
        // Grouped by the provider's own category name, never normalized
        // and never summed.
        let mut by_category: BTreeMap<&str, Vec<&store::BalanceRow>> = BTreeMap::new();
        for row in &history {
            by_category.entry(&row.category).or_default().push(row);
        }
        if by_category.is_empty() {
            out!("  (no balance has ever been read)");
        }
        for (category, rows) in by_category {
            out!("{}", render::balance_line(category, &rows));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_history(profile: &Profile, options: &[String]) -> sumer_store::Result<ExitCode> {
    let mut wanted: Option<String> = None;
    let mut iter = options.iter();
    while let Some(option) = iter.next() {
        match option.as_str() {
            "--resource" => {
                wanted = Some(
                    iter.next()
                        .ok_or_else(|| StoreError::Usage("--resource needs an id".to_owned()))?
                        .clone(),
                );
            }
            other => {
                return Err(StoreError::Usage(format!(
                    "history: unknown option {other:?}"
                )))
            }
        }
    }
    let store = Store::open(profile)?;
    for resource in store::resources(store.conn(), None)? {
        if wanted
            .as_ref()
            .is_some_and(|id| *id != resource.resource_id)
        {
            continue;
        }
        out!(
            "{}/{}  {}",
            resource.adapter_id,
            resource.resource_id,
            resource.label
        );
        let live = sweep::live_records(&store, &resource.adapter_id, &resource.resource_id)?;
        if live.is_empty() {
            out!("  (nothing live)");
        }
        for (local_id, revision, observation) in &live {
            out!("{}", render::history_line(local_id, *revision, observation));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_show(profile: &Profile, options: &[String]) -> sumer_store::Result<ExitCode> {
    let Some(local_id) = options.first() else {
        return Err(StoreError::Usage("show needs a local_id".to_owned()));
    };
    let store = Store::open(profile)?;
    let mut found = false;
    for adapter in store::adapters(store.conn())? {
        let chain = store::chain(store.conn(), &adapter.adapter_id, local_id)?;
        if chain.is_empty() {
            continue;
        }
        found = true;
        let retractions = store::retractions_for(store.conn(), &adapter.adapter_id, local_id)?;
        out!("{}/{local_id}", adapter.adapter_id);
        for line in render::show_lines(&chain, &retractions) {
            out!("{line}");
        }
    }
    if !found {
        out!("no record {local_id:?} in this profile");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(profile: &Profile) -> sumer_store::Result<ExitCode> {
    let store = Store::open(profile)?;
    for adapter in store::adapters(store.conn())? {
        out!(
            "{}  derivation {}  {}",
            adapter.adapter_id,
            adapter
                .local_id_derivation
                .as_deref()
                .unwrap_or("(unknown)"),
            if adapter.needs_reauth {
                "NEEDS REAUTH"
            } else {
                "ok"
            }
        );
        for resource in store::resources(store.conn(), Some(&adapter.adapter_id))? {
            out!(
                "  {}  {}  vantage {}",
                resource.resource_id,
                resource.label,
                resource.last_provider_id.as_deref().unwrap_or("(none yet)")
            );
        }
    }
    out!("\nrecent crawls");
    for crawl in store::crawls(store.conn(), 10)? {
        let verdict = match (&crawl.complete, &crawl.disqualified_reason) {
            (true, _) => "complete".to_owned(),
            (false, Some(reason)) => format!("partial -- {reason}"),
            (false, None) => "open".to_owned(),
        };
        out!(
            "  {} {}/{} at {} -- {verdict}",
            crawl.crawl_id,
            crawl.adapter_id,
            crawl.resource_id,
            crawl.started_at
        );
    }
    let discrepancies = store::discrepancies(store.conn(), 20)?;
    out!("\ndiscrepancies");
    if discrepancies.is_empty() {
        out!("  (none)");
    }
    for discrepancy in &discrepancies {
        out!(
            "  [{}] {}/{} crawl {}: {}",
            discrepancy.kind,
            discrepancy.adapter_id,
            discrepancy.resource_id,
            discrepancy.crawl_id,
            discrepancy.detail
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// One runtime for the whole process. `sumer-host` never starts one -- it
/// only runs inside whatever runtime its caller provides -- so this is
/// that caller.
fn block_on<F: std::future::Future<Output = sumer_store::Result<T>>, T>(
    future: F,
) -> sumer_store::Result<T> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StoreError::Io)?;
    runtime.block_on(future)
}
