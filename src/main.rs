// TODO: somehow better handle rate-limits (https://core.telegram.org/bots/faq#broadcasting-to-users)
//       maybe concat many messages into one (in channel) + queues to properly
//       handle limits

// When index colapses, use `git reset --hard origin/master`
use std::{convert::Infallible, iter, sync::Arc, time::Duration};

use arraylib::Slice;
use either::Either::{Left, Right};
use fntools::{self, value::ValueExt};
use futures::future::{self, pending};
use git2::{Commit, Delta, Diff, DiffOptions, Repository, Sort};
use log::{error, info, warn};
use std::str;
use teloxide::{
    adaptors::{AutoSend, DefaultParseMode},
    prelude::*,
    types::ParseMode,
};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tokio_postgres::NoTls;

use crate::{db::Database, krate::Crate, util::tryn};

mod bot;
mod cfg;
mod db;
mod krate;
mod util;

const VERSION: &str = env!("CARGO_PKG_VERSION");

type Bot = AutoSend<DefaultParseMode<teloxide::Bot>>;

#[tokio::main]
async fn main() {
    assert_eq!(
        unsafe {
            let opt = libgit2_sys::GIT_OPT_SET_MWINDOW_FILE_LIMIT as _;
            libgit2_sys::git_libgit2_opts(opt, 128)
        },
        0,
    );

    let config = Arc::new(cfg::Config::read().expect("couldn't read config"));

    simple_logger::SimpleLogger::new()
        .with_level(config.loglevel)
        .init()
        .expect("Failed to initialize logger");

    info!("starting");

    let db = {
        let (d, conn) = Database::connect(&config.db.cfg(), NoTls)
            .await
            .expect("couldn't connect to the database");

        // docs says to do so
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("Database connection error: {}", e);
            }
        });

        info!("connected to db");
        d
    };

    let index_url = &config.index_url; // Closures still borrow full struct :|
    let index_path = &config.index_path;
    let repo = Repository::open(index_path).unwrap_or_else(move |_| {
        info!("start cloning");
        Repository::clone(index_url, index_path)
            .unwrap()
            .also(|_| info!("cloning finished"))
    });

    let (abortable, abort_handle) = future::abortable(pending::<()>());

    let (tx, mut rx) = mpsc::channel(2);
    let git2_th = {
        let pull_delay = config.pull_delay;
        std::thread::spawn(move || {
            'outer: loop {
                info!("start pulling updates");

                if let Err(err) = pull(&repo, tx.clone()) {
                    error!("couldn't pull new crate version from the index: {}", err);
                }

                info!("pulling updates finished");

                // delay for `config.pull_delay` (default 5 min)
                let mut pd = pull_delay;
                const STEP: Duration = Duration::from_secs(5);

                while pd > Duration::ZERO {
                    if abortable.is_aborted() {
                        break 'outer;
                    }

                    pd = pd.saturating_sub(STEP);
                    std::thread::sleep(STEP);
                }
            }
        })
    };

    let bot = teloxide::Bot::new(&config.bot_token)
        .parse_mode(ParseMode::Html)
        .auto_send();

    let notify_loop = async {
        while let Some((krate, action, _unblock)) = rx.recv().await {
            notify(krate, action, &bot, &db, &config).await;

            // implicitly unblock git2 thread by dropping `_unblock`
        }

        // `recv()` returned `None` => `tx` was dropped => `git2_th` was stopped
        // => `abort_handle.abort()` was probably called
    };

    let tg_loop = async {
        bot::run(bot.clone(), db.clone(), Arc::clone(&config)).await;

        // When bot stopped executing (e.g. because of ^C) stop pull loop
        abort_handle.abort();
    };

    tokio::join!(notify_loop, tg_loop);

    git2_th.join().unwrap();
}

/// Fast-Forward (FF) to a given commit.
///
/// Implementation is taken from <https://stackoverflow.com/a/58778350>.
fn fast_forward(repo: &Repository, commit: &git2::Commit) -> Result<(), git2::Error> {
    let fetch_commit = repo.find_annotated_commit(commit.id())?;
    let analysis = repo.merge_analysis(&[&fetch_commit])?;

    if analysis.0.is_up_to_date() {
        Ok(())
    } else if analysis.0.is_fast_forward() {
        let mut reference = repo.find_reference("refs/heads/master")?;
        reference.set_target(fetch_commit.id(), "Fast-Forward")?;
        repo.set_head(reference.name().unwrap())?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
    } else {
        Err(git2::Error::from_str("Fast-forward only!"))
    }
}

fn pull(
    repo: &Repository,
    ch: Sender<(Crate, ActionKind, oneshot::Sender<Infallible>)>,
) -> Result<(), git2::Error> {
    // fetch changes from remote index
    repo.find_remote("origin")?.fetch(&["master"], None, None)?;

    // Collect all commits in the range `HEAD~1..FETCH_HEAD` (i.e. one before
    // currently checked out to the last fetched)
    let mut walk = repo.revwalk()?;
    walk.push_range("HEAD~1..FETCH_HEAD")?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let commits: Result<Vec<_>, _> = walk.map(|oid| repo.find_commit(oid?)).collect();

    let mut opts = DiffOptions::default();
    let opts = opts.context_lines(0).minimal(true);

    for [prev, next] in Slice::array_windows::<[_; 2]>(&commits?[..]) {
        // Commits from humans tend to be formatted differently, compared to
        // machine-generated ones. This basically makes them unanalyzable.
        if next.author().name() != Some("bors") {
            warn!(
                "Skip commit#{} from non-bors user @{}: {}",
                next.id(),
                next.author().name().unwrap_or("<invalid utf-8>"),
                next.message()
                    .unwrap_or("<invalid utf-8>")
                    .trim_end_matches('\n'),
            );

            continue;
        }

        let diff = repo.diff_tree_to_tree(Some(&prev.tree()?), Some(&next.tree()?), Some(opts))?;
        let (krate, action) = diff_one(diff, (prev, next))?;

        // Send crates.io update to notifier
        let (tx, mut rx) = oneshot::channel();
        ch.blocking_send((krate, action, tx)).ok().unwrap();

        // Wait untill the crate is processed before moving on
        while let Err(oneshot::error::TryRecvError::Empty) = rx.try_recv() {
            // Yeild/sleep to not spend all resources
            std::thread::sleep(Duration::from_secs(1));
        }

        // 'Move' to the next commit
        fast_forward(repo, next)?;
    }

    Ok(())
}

enum ActionKind {
    NewVersion,
    Yanked,
    Unyanked,
}

/// Get a `crates.io` update from a diff of 2 consecutive commits from a
/// `crates.io-index` repository.
fn diff_one(diff: Diff, commits: (&Commit, &Commit)) -> Result<(Crate, ActionKind), git2::Error> {
    let mut prev = None;
    let mut next = None;

    diff.foreach(
        &mut |_, _| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            match delta.status() {
                // New version of a crate or (un)yanked old version
                Delta::Modified | Delta::Added => {
                    assert!(delta.nfiles() == 2 || delta.nfiles() == 1);
                    match line.origin() {
                        '-' => {
                            assert!(
                                prev.is_none(),
                                "Expected number of deletions <= 1 per commit ({} -> {})",
                                commits.0.id(),
                                commits.1.id(),
                            );
                            let krate = str::from_utf8(line.content()).expect("non-utf8 diff");
                            let krate = serde_json::from_str::<Crate>(krate)
                                .expect("cound't deserialize crate");

                            prev = Some(krate);
                        }
                        '+' => {
                            assert!(
                                next.is_none(),
                                "Expected number of additions = 1 per commit ({} -> {})",
                                commits.0.id(),
                                commits.1.id(),
                            );
                            let krate = str::from_utf8(line.content()).expect("non-utf8 diff");
                            let krate = serde_json::from_str::<Crate>(krate)
                                .expect("cound't deserialize crate");

                            next = Some(krate);
                        }
                        _ => { /* don't care */ }
                    }
                }
                delta => {
                    warn!("Unexpected delta: {:?}", delta);
                }
            }

            true
        }),
    )?;

    assert!(
        next.is_some(),
        "Expected number of additions = 1 per commit ({} -> {})",
        commits.0.id(),
        commits.1.id(),
    );
    let next = next.expect("Expected number of additions = 1 per commit");
    match (prev.as_ref().map(|c| c.yanked), next.yanked) {
        /* was yanked?, is yanked? */
        (None, false) => {
            // There were no deleted line & crate is not yanked.
            // New version.
            Ok((next, ActionKind::NewVersion))
        }
        (Some(false), true) => {
            // The crate was not yanked and now is yanked.
            // Crate was yanked.
            Ok((next, ActionKind::Yanked))
        }
        (Some(true), false) => {
            // The crate was yanked and now is not yanked.
            // Crate was unyanked.
            Ok((next, ActionKind::Unyanked))
        }
        _unexpected => {
            // Something unexpected happened
            warn!("Unexpected diff_one input: {:?}, {:?}", next, prev);
            Err(git2::Error::from_str("Unexpected diff"))
        }
    }
}

async fn notify(krate: Crate, action: ActionKind, bot: &Bot, db: &Database, cfg: &cfg::Config) {
    let message = format!(
        "Crate was {action}: <code>{krate}#{version}</code> {links}",
        krate = krate.id.name,
        version = krate.id.vers,
        links = krate.html_links(),
        action = match action {
            ActionKind::NewVersion => "updated",
            ActionKind::Yanked => "yanked",
            ActionKind::Unyanked => "unyanked",
        }
    );

    let channel_fut = async {
        if let Some(chat_id) = cfg.channel {
            if !cfg.ban.crates.contains(krate.id.name.as_str()) {
                notify_inner(bot, chat_id, &message, cfg, &krate, true).await;
            }
        }
    };

    let users_fut = async {
        let users = db
            .list_subscribers(&krate.id.name)
            .await
            .map(Left)
            .map_err(|err| error!("db error while getting subscribers: {}", err))
            .unwrap_or_else(|_| Right(iter::empty()));

        for chat_id in users {
            notify_inner(bot, chat_id, &message, cfg, &krate, false).await;
            tokio::time::sleep(cfg.broadcast_delay_millis.into()).await;
        }
    };

    tokio::join!(channel_fut, users_fut);
}

async fn notify_inner(
    bot: &Bot,
    chat_id: i64,
    msg: &str,
    cfg: &cfg::Config,
    krate: &Crate,
    quiet: bool,
) {
    tryn(5, cfg.retry_delay.0, || {
        bot.send_message(chat_id, msg)
            .disable_web_page_preview(true)
            .disable_notification(quiet)
    })
    .await
    .map(drop)
    .unwrap_or_else(|err| {
        error!(
            "error while trying to send notification about {krate:?} to {chat_id}: {err}",
            krate = krate,
            chat_id = chat_id,
            err = err
        );
    });
}
