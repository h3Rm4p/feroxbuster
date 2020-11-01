use crossterm::event::{self, Event, KeyCode};
use feroxbuster::{
    banner,
    config::{CONFIGURATION, PROGRESS_PRINTER},
    heuristics, logger, reporter,
    scanner::{scan_url, PAUSE_SCAN},
    utils::{ferox_print, get_current_depth, module_colorizer, status_colorizer},
    FeroxResponse, FeroxResult, SLEEP_DURATION, VERSION,
};
use futures::StreamExt;
use std::{
    collections::HashSet,
    fs::File,
    io::{stderr, BufRead, BufReader},
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{io, sync::mpsc::UnboundedSender};
use tokio_util::codec::{FramedRead, LinesCodec};

/// Atomic boolean flag, used to determine whether or not the terminal input handler should exit
pub static SCAN_COMPLETE: AtomicBool = AtomicBool::new(false);

/// Handles specific key events triggered by the user over stdin
fn terminal_input_handler() {
    log::trace!("enter: terminal_input_handler");

    loop {
        if event::poll(Duration::from_millis(SLEEP_DURATION)).unwrap_or(false) {
            // It's guaranteed that the `read()` won't block when the `poll()`
            // function returns `true`

            if let Ok(key_pressed) = event::read() {
                if key_pressed == Event::Key(KeyCode::Enter.into()) {
                    // if the user presses Enter, toggle the value stored in PAUSE_SCAN
                    // ignore any other keys
                    let current = PAUSE_SCAN.load(Ordering::Acquire);

                    PAUSE_SCAN.store(!current, Ordering::Release);
                }
            }
        } else {
            // Timeout expired and no `Event` is available; use the timeout to check SCAN_COMPLETE
            if SCAN_COMPLETE.load(Ordering::Relaxed) {
                // scan has been marked complete by main, time to exit the loop
                break;
            }
        }
    }
    log::trace!("exit: terminal_input_handler");
}

/// Create a HashSet of Strings from the given wordlist then stores it inside an Arc
fn get_unique_words_from_wordlist(path: &str) -> FeroxResult<Arc<HashSet<String>>> {
    log::trace!("enter: get_unique_words_from_wordlist({})", path);

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{} {} {}",
                status_colorizer("ERROR"),
                module_colorizer("main::get_unique_words_from_wordlist"),
                e
            );
            log::error!("Could not open wordlist: {}", e);
            log::trace!("exit: get_unique_words_from_wordlist -> {}", e);

            return Err(Box::new(e));
        }
    };

    let reader = BufReader::new(file);

    let mut words = HashSet::new();

    for line in reader.lines() {
        let result = line?;

        if result.starts_with('#') || result.is_empty() {
            continue;
        }

        words.insert(result);
    }

    log::trace!(
        "exit: get_unique_words_from_wordlist -> Arc<wordlist[{} words...]>",
        words.len()
    );

    Ok(Arc::new(words))
}

/// Determine whether it's a single url scan or urls are coming from stdin, then scan as needed
async fn scan(
    targets: Vec<String>,
    tx_term: UnboundedSender<FeroxResponse>,
    tx_file: UnboundedSender<String>,
) -> FeroxResult<()> {
    log::trace!("enter: scan({:?}, {:?}, {:?})", targets, tx_term, tx_file);
    // cloning an Arc is cheap (it's basically a pointer into the heap)
    // so that will allow for cheap/safe sharing of a single wordlist across multi-target scans
    // as well as additional directories found as part of recursion
    let words =
        tokio::spawn(async move { get_unique_words_from_wordlist(&CONFIGURATION.wordlist) })
            .await??;

    if words.len() == 0 {
        eprintln!(
            "{} {} Did not find any words in {}",
            status_colorizer("ERROR"),
            module_colorizer("main::scan"),
            CONFIGURATION.wordlist
        );
        process::exit(1);
    }

    let mut tasks = vec![];

    for target in targets {
        let word_clone = words.clone();
        let term_clone = tx_term.clone();
        let file_clone = tx_file.clone();

        let task = tokio::spawn(async move {
            let base_depth = get_current_depth(&target);
            scan_url(&target, word_clone, base_depth, term_clone, file_clone).await;
        });

        tasks.push(task);
    }

    // drive execution of all accumulated futures
    futures::future::join_all(tasks).await;
    log::trace!("exit: scan");

    Ok(())
}

async fn get_targets() -> FeroxResult<Vec<String>> {
    log::trace!("enter: get_targets");

    let mut targets = vec![];

    if CONFIGURATION.stdin {
        // got targets from stdin, i.e. cat sites | ./feroxbuster ...
        // just need to read the targets from stdin and spawn a future for each target found
        let stdin = io::stdin(); // tokio's stdin, not std
        let mut reader = FramedRead::new(stdin, LinesCodec::new());

        while let Some(line) = reader.next().await {
            targets.push(line?);
        }
    } else {
        targets.push(CONFIGURATION.target_url.clone());
    }

    log::trace!("exit: get_targets -> {:?}", targets);

    Ok(targets)
}

#[tokio::main]
async fn main() {
    // setup logging based on the number of -v's used
    logger::initialize(CONFIGURATION.verbosity);

    // can't trace main until after logger is initialized
    log::trace!("enter: main");
    log::debug!("{:#?}", *CONFIGURATION);

    // spawn a thread that listens for keyboard input on stdin, when a user presses enter
    // the input handler will toggle PAUSE_SCAN, which in turn is used to pause and resume
    // scans that are already running
    tokio::task::spawn_blocking(terminal_input_handler);

    let save_output = !CONFIGURATION.output.is_empty(); // was -o used?

    let (tx_term, tx_file, term_handle, file_handle) =
        reporter::initialize(&CONFIGURATION.output, save_output);

    // get targets from command line or stdin
    let targets = match get_targets().await {
        Ok(t) => t,
        Err(e) => {
            // should only happen in the event that there was an error reading from stdin
            log::error!("{}", e);
            ferox_print(
                &format!(
                    "{} {} {}",
                    status_colorizer("ERROR"),
                    module_colorizer("main::get_targets"),
                    e
                ),
                &PROGRESS_PRINTER,
            );
            process::exit(1);
        }
    };

    if !CONFIGURATION.quiet {
        // only print banner if -q isn't used
        let std_stderr = stderr(); // std::io::stderr
        banner::initialize(&targets, &CONFIGURATION, &VERSION, std_stderr).await;
    }

    // discard non-responsive targets
    let live_targets = heuristics::connectivity_test(&targets).await;

    // kick off a scan against any targets determined to be responsive
    match scan(live_targets, tx_term.clone(), tx_file.clone()).await {
        Ok(_) => {
            log::info!("All scans complete!");
        }
        Err(e) => log::error!("An error occurred: {}", e),
    };

    // manually drop tx in order for the rx task's while loops to eval to false
    drop(tx_term);
    log::trace!("dropped terminal output handler's transmitter");

    log::trace!("awaiting terminal output handler's receiver");
    // after dropping tx, we can await the future where rx lived
    match term_handle.await {
        Ok(_) => {}
        Err(e) => {
            log::error!("error awaiting terminal output handler's receiver: {}", e);
        }
    }
    log::trace!("done awaiting terminal output handler's receiver");

    log::trace!("tx_file: {:?}", tx_file);
    // the same drop/await process used on the terminal handler is repeated for the file handler
    // we drop the file transmitter every time, because it's created no matter what
    drop(tx_file);

    log::trace!("dropped file output handler's transmitter");
    if save_output {
        // but we only await if -o was specified
        log::trace!("awaiting file output handler's receiver");
        match file_handle.unwrap().await {
            Ok(_) => {}
            Err(e) => {
                log::error!("error awaiting file output handler's receiver: {}", e);
            }
        }
        log::trace!("done awaiting file output handler's receiver");
    }

    // mark all scans complete so the terminal input handler will exit cleanly
    SCAN_COMPLETE.store(true, Ordering::Relaxed);

    log::trace!("exit: main");

    // clean-up function for the MultiProgress bar; must be called last in order to still see
    // the final trace message above
    PROGRESS_PRINTER.finish();
}
