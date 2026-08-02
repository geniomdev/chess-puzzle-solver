use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_DATABASE_PATH: &str = "data/lichess_db_puzzle.csv";
const DEFAULT_SEARCH_DEPTH: u8 = 16;
const DEFAULT_PUZZLE_COUNT: usize = 100;
const DEFAULT_HASH: usize = 16;
const READ_BUFFER_BYTES: usize = 1 << 20;
const USAGE: &str = "usage: cps --engine \"COMMAND [ARGUMENTS]\" [--database PATH] [--depth PLIES] [--count NUMBER] [--skip NUMBER] [--workers NUMBER] [--hash MEGABYTES] [--option NAME=VALUE] [--show-failed]";

struct Puzzle {
    id: String,
    fen: String,
    opponent_move: String,
    solution: Vec<String>,
    rating: u32,
    url: String,
}

#[derive(Debug, PartialEq)]
struct EngineOption {
    name: String,
    value: String,
    demanded: bool,
}

struct Settings {
    engine: String,
    database_path: PathBuf,
    search_depth: u8,
    puzzle_count: usize,
    skip_count: usize,
    workers: usize,
    engine_options: Vec<EngineOption>,
    show_failed: bool,
}

fn available_cores() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: String::new(),
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            search_depth: DEFAULT_SEARCH_DEPTH,
            puzzle_count: DEFAULT_PUZZLE_COUNT,
            skip_count: 0,
            workers: available_cores(),
            engine_options: vec![EngineOption {
                name: "Hash".to_string(),
                value: DEFAULT_HASH.to_string(),
                demanded: false,
            }],
            show_failed: false,
        }
    }
}

impl Settings {
    fn set_option(&mut self, name: &str, value: &str) {
        match self
            .engine_options
            .iter_mut()
            .find(|option| option.name == name)
        {
            Some(option) => {
                value.clone_into(&mut option.value);
                option.demanded = true;
            }
            None => self.engine_options.push(EngineOption {
                name: name.to_string(),
                value: value.to_string(),
                demanded: true,
            }),
        }
    }
}

fn parse_settings(arguments: impl IntoIterator<Item = String>) -> io::Result<Settings> {
    fn invalid(message: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{message}\n{USAGE}"))
    }

    fn value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
        arguments
            .next()
            .ok_or_else(|| invalid(format!("{flag} needs a value")))
    }

    fn number<T: TryFrom<u64>>(
        arguments: &mut impl Iterator<Item = String>,
        flag: &str,
        least: u64,
        expectation: &str,
    ) -> io::Result<T> {
        let text = value(arguments, flag)?;
        let counted: u64 = text
            .parse()
            .map_err(|_| invalid(format!("{flag} needs a number, not `{text}`")))?;

        T::try_from(counted)
            .ok()
            .filter(|_| counted >= least)
            .ok_or_else(|| invalid(format!("{flag} needs {expectation}")))
    }

    let mut settings = Settings::default();
    let mut arguments = arguments.into_iter();

    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--engine" => {
                settings.engine = value(&mut arguments, &flag)?;
                if settings.engine.split_whitespace().next().is_none() {
                    return Err(invalid(format!("{flag} needs a command")));
                }
            }
            "--database" => settings.database_path = PathBuf::from(value(&mut arguments, &flag)?),
            "--depth" => {
                settings.search_depth =
                    number(&mut arguments, &flag, 1, "a depth between 1 and 255 plies")?;
            }
            "--count" => {
                settings.puzzle_count = number(&mut arguments, &flag, 1, "at least one puzzle")?;
            }
            "--skip" => {
                settings.skip_count = number(&mut arguments, &flag, 0, "a number of puzzles")?;
            }
            "--workers" => {
                settings.workers = number(&mut arguments, &flag, 1, "at least one engine")?;
            }
            "--hash" => {
                let megabytes: usize = number(&mut arguments, &flag, 1, "at least one megabyte")?;
                settings.set_option("Hash", &megabytes.to_string());
            }
            "--option" => {
                let assignment = value(&mut arguments, &flag)?;
                let (name, option_value) = assignment
                    .split_once('=')
                    .filter(|(name, _)| !name.trim().is_empty())
                    .ok_or_else(|| {
                        invalid(format!("{flag} needs NAME=VALUE, not `{assignment}`"))
                    })?;
                settings.set_option(name.trim(), option_value);
            }
            "--show-failed" => settings.show_failed = true,
            _ => return Err(invalid(format!("unknown flag `{flag}`"))),
        }
    }

    if settings.engine.is_empty() {
        return Err(invalid("--engine is required"));
    }

    Ok(settings)
}

struct Engine {
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    line: String,
    skipped_options: Vec<String>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
    }
}

fn parse_option_name(line: &str) -> Option<&str> {
    let declaration = line.trim().strip_prefix("option ")?.trim_start();
    let named = declaration.strip_prefix("name ")?;
    let end = named.find(" type ")?;

    Some(named[..end].trim())
}

impl Engine {
    fn start(command: &str, options: &[EngineOption]) -> io::Result<Self> {
        let mut words = command.split_whitespace();
        let program = words.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--engine needs a command")
        })?;

        let mut process = Command::new(program)
            .args(words)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| io::Error::new(error.kind(), format!("{program}: {error}")))?;

        let mut engine = Self {
            input: process.stdin.take().expect("stdin is piped"),
            output: BufReader::new(process.stdout.take().expect("stdout is piped")),
            process,
            line: String::new(),
            skipped_options: Vec::new(),
        };

        let mut announced = HashSet::new();
        engine.send("uci")?;
        engine.wait_for_each("uciok", |line| {
            if let Some(name) = parse_option_name(line) {
                announced.insert(name.to_string());
            }
        })?;

        engine.set_options(&announced, options)?;
        engine.send("isready")?;
        engine.wait_for("readyok")?;

        Ok(engine)
    }

    fn set_options(
        &mut self,
        announced: &HashSet<String>,
        options: &[EngineOption],
    ) -> io::Result<()> {
        for option in options {
            if !announced.contains(&option.name) {
                if option.demanded {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("engine does not support option `{}`", option.name),
                    ));
                }
                self.skipped_options.push(option.name.clone());
                continue;
            }
            self.send(&format!(
                "setoption name {} value {}",
                option.name, option.value
            ))?;
        }

        Ok(())
    }

    fn send(&mut self, command: &str) -> io::Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()
    }

    fn wait_for(&mut self, token: &str) -> io::Result<String> {
        self.wait_for_each(token, |_| ())
    }

    fn wait_for_each(&mut self, token: &str, mut read: impl FnMut(&str)) -> io::Result<String> {
        loop {
            self.line.clear();
            if self.output.read_line(&mut self.line)? == 0 {
                return Err(io::Error::other(format!("engine stopped before `{token}`")));
            }
            if self.line.split_whitespace().next() == Some(token) {
                return Ok(self.line.clone());
            }
            read(&self.line);
        }
    }

    fn new_game(&mut self) -> io::Result<()> {
        self.send("ucinewgame")?;
        self.send("isready")?;
        self.wait_for("readyok").map(|_| ())
    }

    fn go(&mut self, fen: &str, moves: &[String], depth: u8) -> io::Result<()> {
        self.send(&format!("position fen {fen} moves {}", moves.join(" ")))?;
        self.send(&format!("go depth {depth}"))
    }

    fn best_move(&mut self, fen: &str, moves: &[String], depth: u8) -> io::Result<String> {
        self.go(fen, moves, depth)?;

        let answer = self.wait_for("bestmove")?;
        match answer.split_whitespace().nth(1) {
            Some(played) if played != "(none)" => Ok(played.to_string()),
            _ => Err(io::Error::other(format!(
                "engine answered `{}`",
                answer.trim()
            ))),
        }
    }

    fn is_checkmate(&mut self, fen: &str, moves: &[String]) -> io::Result<bool> {
        self.go(fen, moves, 1)?;

        let mut mate_score = false;
        let answer = self.wait_for_each("bestmove", |line| {
            mate_score |= line.contains("score mate 0");
        })?;

        Ok(mate_score && answer.contains("(none)"))
    }

    fn quit(mut self) -> io::Result<()> {
        self.send("quit")?;
        self.process.wait().map(|_| ())
    }
}

fn parse_puzzle(line: &str) -> Option<Puzzle> {
    let line = line.trim_end();
    let (id, rest) = line.split_once(',')?;
    let (fen, rest) = rest.split_once(',')?;
    let (moves, rest) = rest.split_once(',')?;
    let (rating, fields_after_rating) = rest.split_once(',').unwrap_or((rest, ""));

    let (opponent_move, solution) = moves.trim().split_once(' ')?;
    let solution: Vec<String> = solution.split_whitespace().map(str::to_string).collect();
    if solution.is_empty() {
        return None;
    }

    Some(Puzzle {
        id: id.to_string(),
        fen: fen.to_string(),
        opponent_move: opponent_move.to_string(),
        solution,
        rating: rating.parse().ok()?,
        url: fields_after_rating
            .split(',')
            .nth(4)
            .unwrap_or_default()
            .to_string(),
    })
}

struct PuzzleSource<'a> {
    settings: &'a Settings,
    reader: BufReader<File>,
    line: String,
    skipped: usize,
    taken: usize,
    stopped: bool,
}

impl<'a> PuzzleSource<'a> {
    fn open(settings: &'a Settings) -> io::Result<Self> {
        let path = &settings.database_path;
        let file = File::open(path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;

        let mut source = Self {
            settings,
            reader: BufReader::with_capacity(READ_BUFFER_BYTES, file),
            line: String::new(),
            skipped: 0,
            taken: 0,
            stopped: false,
        };
        source.read_line()?;

        Ok(source)
    }

    fn read_line(&mut self) -> io::Result<bool> {
        self.line.clear();
        Ok(self.reader.read_line(&mut self.line)? > 0)
    }

    fn skip_ahead(&mut self) -> io::Result<()> {
        while self.skipped < self.settings.skip_count {
            if !self.read_line()? {
                break;
            }
            if parse_puzzle(&self.line).is_some() {
                self.skipped += 1;
            }
        }

        Ok(())
    }

    fn next_puzzle(&mut self) -> io::Result<Option<Puzzle>> {
        while !self.stopped && self.taken < self.settings.puzzle_count {
            if !self.read_line()? {
                break;
            }
            let Some(puzzle) = parse_puzzle(&self.line) else {
                continue;
            };

            self.taken += 1;

            return Ok(Some(puzzle));
        }

        Ok(None)
    }

    fn stop(&mut self) {
        self.stopped = true;
    }

    fn shortfall(&self) -> io::Result<()> {
        if self.taken >= self.settings.puzzle_count {
            return Ok(());
        }

        let skipped = if self.skipped == 0 {
            String::new()
        } else {
            format!(" after skipping {}", self.skipped)
        };

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: read {} puzzles{skipped}, expected {}",
                self.settings.database_path.display(),
                self.taken,
                self.settings.puzzle_count
            ),
        ))
    }
}

const CLEARED_LINE: &str = "\r\x1b[K";
const LINE_ABOVE: &str = "\x1b[A";
const BLANK_LINE: &str = "\n";
const UNSOLVED_HEADING: &str = "Unsolved puzzles:\n";

#[derive(Default)]
struct Progress {
    tested: usize,
    solved: usize,
    listed_unsolved: bool,
    drawn_in_place: bool,
}

impl Progress {
    fn line(&self, total: usize, elapsed: Duration) -> Option<String> {
        (self.tested > 0).then(|| {
            format!(
                "puzzle {} of {total} ({:.1}%), solved {} ({:.1}%) in {:.1}s",
                self.tested,
                100.0 * self.tested as f64 / total as f64,
                self.solved,
                100.0 * self.solved as f64 / self.tested as f64,
                elapsed.as_secs_f64()
            )
        })
    }

    fn unsolved_report(&mut self, puzzle: &Puzzle, played: &str, search_depth: u8) -> String {
        let opening = if self.drawn_in_place {
            format!("{CLEARED_LINE}{LINE_ABOVE}{CLEARED_LINE}")
        } else {
            String::new()
        };
        let heading = if self.listed_unsolved {
            ""
        } else {
            UNSOLVED_HEADING
        };
        self.listed_unsolved = true;
        self.drawn_in_place = false;

        format!(
            "{opening}{heading}{}",
            failed_puzzle_line(puzzle, played, search_depth)
        )
    }

    fn opening(&self) -> &'static str {
        if self.drawn_in_place {
            CLEARED_LINE
        } else {
            BLANK_LINE
        }
    }
}

struct Run<'a> {
    settings: &'a Settings,
    source: Mutex<PuzzleSource<'a>>,
    progress: Mutex<Progress>,
    redraws_progress: bool,
    started: Instant,
}

impl<'a> Run<'a> {
    fn new(settings: &'a Settings, source: PuzzleSource<'a>) -> Self {
        Self {
            settings,
            source: Mutex::new(source),
            progress: Mutex::new(Progress::default()),
            redraws_progress: io::stdout().is_terminal(),
            started: Instant::now(),
        }
    }

    fn take_puzzle(&self) -> io::Result<Option<Puzzle>> {
        self.source.lock().unwrap().next_puzzle()
    }

    fn solve_share(&self, checked: Option<Engine>) -> io::Result<()> {
        let mut engine = match checked {
            Some(engine) => engine,
            None => Engine::start(&self.settings.engine, &self.settings.engine_options)?,
        };

        while let Some(puzzle) = self.take_puzzle()? {
            engine.new_game()?;

            let mut moves = vec![puzzle.opponent_move.clone()];
            let played = engine.best_move(&puzzle.fen, &moves, self.settings.search_depth)?;

            let solved = played == puzzle.solution[0] || {
                moves.push(played.clone());
                engine.is_checkmate(&puzzle.fen, &moves)?
            };

            self.report(&puzzle, (!solved).then_some(played))?;
        }

        engine.quit()
    }

    fn report(&self, puzzle: &Puzzle, failure: Option<String>) -> io::Result<()> {
        let mut progress = self.progress.lock().unwrap();

        progress.tested += 1;
        match failure {
            None => progress.solved += 1,
            Some(played) => {
                if self.settings.show_failed {
                    let report =
                        progress.unsolved_report(puzzle, &played, self.settings.search_depth);
                    println!("{report}");
                }
            }
        }

        if !self.redraws_progress {
            return Ok(());
        }
        let Some(line) = progress.line(self.settings.puzzle_count, self.started.elapsed()) else {
            return Ok(());
        };

        print!("{}{line}", progress.opening());
        progress.drawn_in_place = true;
        io::stdout().flush()
    }

    fn solve(&self, workers: usize, checked: Engine) -> io::Result<()> {
        let mut checked = Some(checked);
        thread::scope(|scope| {
            let shares: Vec<_> = (0..workers)
                .map(|_| {
                    let engine = checked.take();
                    scope.spawn(move || {
                        self.solve_share(engine)
                            .inspect_err(|_| self.source.lock().unwrap().stop())
                    })
                })
                .collect();

            shares.into_iter().try_for_each(|share| {
                share
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
        })
    }

    fn finish(self) -> io::Result<()> {
        let progress = self.progress.into_inner().unwrap();
        if let Some(line) = progress.line(self.settings.puzzle_count, self.started.elapsed()) {
            println!("{}{line}", progress.opening());
        }

        self.source.into_inner().unwrap().shortfall()
    }
}

fn startup_banner(settings: &Settings, workers: usize, skipped: &[String]) -> String {
    let options: Vec<String> = settings
        .engine_options
        .iter()
        .filter(|option| !skipped.contains(&option.name))
        .map(|option| format!("{}={}", option.name, option.value))
        .collect();

    let listed = if options.is_empty() {
        String::new()
    } else {
        format!(" with {}", options.join(" "))
    };

    format!(
        "{} at Depth {}{listed} in {workers} {}",
        settings.engine,
        settings.search_depth,
        match workers {
            1 => "process",
            _ => "processes",
        }
    )
}

fn failed_puzzle_line(puzzle: &Puzzle, played: &str, search_depth: u8) -> String {
    let line = format!(
        "{} | {} | {} {} | {:>4} | played {played} | {}",
        puzzle.id,
        puzzle.fen,
        puzzle.opponent_move,
        puzzle.solution.join(" "),
        puzzle.rating,
        puzzle.url
    );

    let plies = puzzle.solution.len();
    if plies > usize::from(search_depth) {
        format!("{line} | needs depth {plies}")
    } else {
        line
    }
}

fn solve_puzzles(settings: &Settings) -> io::Result<()> {
    let mut source = PuzzleSource::open(settings)?;
    let checked = Engine::start(&settings.engine, &settings.engine_options)?;
    let workers = settings.workers.min(settings.puzzle_count);

    for name in &checked.skipped_options {
        eprintln!("cps: engine does not support option `{name}`, keeping the engine default");
    }
    println!(
        "{}",
        startup_banner(settings, workers, &checked.skipped_options)
    );

    source.skip_ahead()?;

    let run = Run::new(settings, source);
    let outcome = run.solve(workers, checked);

    outcome.and(run.finish())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match parse_settings(arguments).and_then(|settings| solve_puzzles(&settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cps: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const PUZZLE_LINE: &str = "00008,r6k/pp2r2p/4Rp1Q/3p4/8/1N1P2R1/PqP2bPP/7K b - - 0 24,f2g3 e6e7 b2b1,1784,77,95,9822,crushing long,https://lichess.org/787zsVup/black#48,";
    const HEADER_LINE: &str =
        "PuzzleId,FEN,Moves,Rating,RatingDeviation,Popularity,NbPlays,Themes,GameUrl,OpeningTags";

    fn flags(arguments: &[&str]) -> io::Result<Settings> {
        parse_settings(arguments.iter().map(ToString::to_string))
    }

    fn rejection(arguments: &[&str]) -> io::Error {
        flags(arguments).err().unwrap()
    }

    fn demanded(name: &str, value: &str) -> EngineOption {
        EngineOption {
            name: name.to_string(),
            value: value.to_string(),
            demanded: true,
        }
    }

    fn defaulted(name: &str, value: &str) -> EngineOption {
        EngineOption {
            demanded: false,
            ..demanded(name, value)
        }
    }

    fn temp_path(name: &str, extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cps-{}-{name}.{extension}", std::process::id()))
    }

    fn read_puzzles(settings: &Settings) -> io::Result<Vec<Puzzle>> {
        let mut source = PuzzleSource::open(settings)?;
        source.skip_ahead()?;
        let mut puzzles = Vec::new();
        while let Some(puzzle) = source.next_puzzle()? {
            puzzles.push(puzzle);
        }
        source.shortfall()?;

        Ok(puzzles)
    }

    fn database(name: &str, contents: &str) -> Settings {
        let path = temp_path(name, "csv");
        fs::write(&path, contents).unwrap();

        Settings {
            database_path: path,
            puzzle_count: 2,
            ..Settings::default()
        }
    }

    #[test]
    fn splits_opponent_move_from_solution() {
        let puzzle = parse_puzzle(PUZZLE_LINE).unwrap();

        assert_eq!(
            puzzle.fen,
            "r6k/pp2r2p/4Rp1Q/3p4/8/1N1P2R1/PqP2bPP/7K b - - 0 24"
        );
        assert_eq!(puzzle.id, "00008");
        assert_eq!(puzzle.rating, 1784);
        assert_eq!(puzzle.opponent_move, "f2g3");
        assert_eq!(puzzle.solution, ["e6e7", "b2b1"]);
        assert_eq!(puzzle.url, "https://lichess.org/787zsVup/black#48");
    }

    #[test]
    fn keeps_defaults_beside_the_engine() {
        let settings = flags(&["--engine", "lc0"]).unwrap();

        assert_eq!(settings.database_path, PathBuf::from(DEFAULT_DATABASE_PATH));
        assert_eq!(settings.search_depth, 16);
        assert_eq!(settings.puzzle_count, 100);
        assert_eq!(settings.skip_count, 0);
        assert_eq!(settings.workers, available_cores());
        assert_eq!(settings.engine_options, [defaulted("Hash", "16")]);
        assert!(!settings.show_failed);
    }

    #[test]
    fn replaces_the_default_hash_instead_of_repeating_it() {
        for arguments in [
            ["--engine", "lc0", "--hash", "256"],
            ["--engine", "lc0", "--option", "Hash=256"],
        ] {
            let settings = flags(&arguments).unwrap();

            assert_eq!(settings.engine_options, [demanded("Hash", "256")]);
        }
    }

    #[test]
    fn rejects_flags_it_cannot_follow() {
        for (arguments, message) in [
            (&["--depth", "8"][..], "--engine is required"),
            (&["--wat"], "unknown flag `--wat`"),
            (&["--depth"], "--depth needs a value"),
            (
                &["--depth", "0"],
                "--depth needs a depth between 1 and 255 plies",
            ),
            (
                &["--depth", "256"],
                "--depth needs a depth between 1 and 255 plies",
            ),
            (&["--count", "0"], "--count needs at least one puzzle"),
            (&["--count", "many"], "--count needs a number, not `many`"),
            (
                &["--engine", "lc0", "--workers", "0"],
                "--workers needs at least one engine",
            ),
            (
                &["--engine", "lc0", "--hash", "0"],
                "--hash needs at least one megabyte",
            ),
            (
                &["--engine", "lc0", "--option", "Threads"],
                "--option needs NAME=VALUE, not `Threads`",
            ),
            (
                &["--engine", "lc0", "--option", "=4"],
                "--option needs NAME=VALUE, not `=4`",
            ),
        ] {
            let error = rejection(arguments);

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(message), "{arguments:?}");
        }
    }

    #[test]
    fn takes_every_flag() {
        let settings = flags(&[
            "--engine",
            "lc0",
            "--database",
            "puzzles.csv",
            "--depth",
            "8",
            "--count",
            "3",
            "--skip",
            "7",
            "--workers",
            "4",
            "--hash",
            "256",
            "--option",
            "MultiPV=2",
            "--show-failed",
        ])
        .unwrap();

        assert_eq!(settings.engine, "lc0");
        assert_eq!(settings.database_path, PathBuf::from("puzzles.csv"));
        assert_eq!(settings.search_depth, 8);
        assert_eq!(settings.puzzle_count, 3);
        assert_eq!(settings.skip_count, 7);
        assert_eq!(settings.workers, 4);
        assert_eq!(
            settings.engine_options,
            [demanded("Hash", "256"), demanded("MultiPV", "2")]
        );
        assert!(settings.show_failed);
    }

    #[test]
    fn keeps_option_names_with_spaces_and_values_with_equals() {
        let settings = flags(&[
            "--engine",
            "lc0",
            "--option",
            "Debug Log File=logs/a=b.txt",
            "--option",
            "SyzygyPath=",
        ])
        .unwrap();

        assert_eq!(
            settings.engine_options,
            [
                defaulted("Hash", "16"),
                demanded("Debug Log File", "logs/a=b.txt"),
                demanded("SyzygyPath", ""),
            ]
        );
    }

    #[test]
    fn rejects_lines_that_are_not_puzzles() {
        for line in [
            "PuzzleId,FEN,Moves,Rating",
            "00008,8/8/8/8/8/8/8/K6k w - - 0 1,f2g3,1784",
            "00008,8/8/8/8/8/8/8/K6k w - - 0 1,f2g3 e6e7,high",
            "",
        ] {
            assert!(parse_puzzle(line).is_none(), "{line}");
        }
    }

    #[test]
    fn reads_no_more_puzzles_than_requested() {
        let settings = database(
            "three-rows",
            &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n"),
        );

        let puzzles = read_puzzles(&settings).unwrap();

        assert_eq!(puzzles.len(), 2);
        assert_eq!(puzzles[0].opponent_move, "f2g3");
    }

    #[test]
    fn skips_the_first_line_even_when_it_parses() {
        let first = PUZZLE_LINE.replace("f2g3", "a1a2");
        let second = PUZZLE_LINE.replace("f2g3", "b1b2");
        let third = PUZZLE_LINE.replace("f2g3", "c1c2");
        let settings = database("first-line", &format!("{first}\n{second}\n{third}\n"));

        let puzzles = read_puzzles(&settings).unwrap();

        assert_eq!(puzzles[0].opponent_move, "b1b2");
        assert_eq!(puzzles[1].opponent_move, "c1c2");
    }

    #[test]
    fn starts_reading_after_the_skipped_puzzles() {
        let rows: Vec<String> = ["a1a1", "b2b2", "c3c3", "d4d4"]
            .iter()
            .map(|first_move| PUZZLE_LINE.replace("f2g3", first_move))
            .collect();
        let mut settings = database("skip", &format!("{HEADER_LINE}\n{}\n", rows.join("\n")));
        settings.skip_count = 2;

        let puzzles = read_puzzles(&settings).unwrap();

        assert_eq!(puzzles.len(), 2);
        assert_eq!(puzzles[0].opponent_move, "c3c3");
        assert_eq!(puzzles[1].opponent_move, "d4d4");
    }

    #[test]
    fn fails_when_skipping_leaves_too_few_puzzles() {
        let mut settings = database(
            "skip-everything",
            &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n"),
        );
        settings.skip_count = 2;

        let error = read_puzzles(&settings).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("read 1 puzzles after skipping 2, expected 2")
        );
    }

    #[test]
    fn fails_when_database_holds_fewer_puzzles() {
        let settings = database("one-row", &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n"));

        let error = read_puzzles(&settings).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("read 1 puzzles, expected 2"));
    }

    #[test]
    fn fails_when_database_is_missing() {
        let settings = Settings {
            database_path: PathBuf::from("no/such/database.csv"),
            ..Settings::default()
        };

        let error = read_puzzles(&settings).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no/such/database.csv"));
    }

    #[test]
    fn fails_when_engine_is_missing() {
        let error = Engine::start("no-such-engine", &[]).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no-such-engine"));
    }

    #[test]
    fn reads_option_names_out_of_the_uci_answer() {
        assert_eq!(
            parse_option_name("option name Hash type spin default 16 min 1 max 1024"),
            Some("Hash")
        );
        assert_eq!(
            parse_option_name("option name Debug Log File type string default"),
            Some("Debug Log File")
        );
        assert_eq!(parse_option_name("id name Stockfish 17"), None);
        assert_eq!(parse_option_name("uciok"), None);
    }

    #[test]
    fn rejects_empty_engine_command() {
        for error in [
            flags(&["--engine", "   "]).err().unwrap(),
            Engine::start("   ", &[]).err().unwrap(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("--engine needs a command"));
        }
    }

    #[test]
    fn shows_progress_as_two_percentages() {
        let progress = Progress {
            tested: 3,
            solved: 2,
            ..Progress::default()
        };

        assert_eq!(
            progress.line(10, Duration::from_millis(12_340)).unwrap(),
            "puzzle 3 of 10 (30.0%), solved 2 (66.7%) in 12.3s"
        );
    }

    #[test]
    fn heads_the_unsolved_list_once() {
        let puzzle = parse_puzzle(PUZZLE_LINE).unwrap();
        let mut progress = Progress::default();

        let first = progress.unsolved_report(&puzzle, "d4d5", 16);
        let second = progress.unsolved_report(&puzzle, "d4d5", 16);

        assert_eq!(
            first,
            format!(
                "{UNSOLVED_HEADING}{}",
                failed_puzzle_line(&puzzle, "d4d5", 16)
            )
        );
        assert_eq!(second, failed_puzzle_line(&puzzle, "d4d5", 16));
    }

    #[test]
    fn keeps_a_blank_line_above_the_progress_line() {
        let puzzle = parse_puzzle(PUZZLE_LINE).unwrap();
        let mut progress = Progress::default();

        assert_eq!(progress.opening(), BLANK_LINE);
        assert!(
            progress
                .unsolved_report(&puzzle, "d4d5", 16)
                .starts_with(UNSOLVED_HEADING)
        );

        progress.drawn_in_place = true;

        assert_eq!(progress.opening(), CLEARED_LINE);
        assert!(
            progress
                .unsolved_report(&puzzle, "d4d5", 16)
                .starts_with(&format!("{CLEARED_LINE}{LINE_ABOVE}{CLEARED_LINE}"))
        );
        assert_eq!(progress.opening(), BLANK_LINE);
    }

    #[test]
    fn shows_no_progress_before_the_first_puzzle_is_tested() {
        assert!(Progress::default().line(10, Duration::ZERO).is_none());
    }

    #[test]
    fn writes_the_banner_and_the_failed_puzzles() {
        let settings = Settings {
            engine: "lc0".to_string(),
            search_depth: 12,
            ..Settings::default()
        };
        let puzzle = Puzzle {
            rating: 784,
            ..parse_puzzle(PUZZLE_LINE).unwrap()
        };

        assert_eq!(
            startup_banner(&settings, 1, &[]),
            "lc0 at Depth 12 with Hash=16 in 1 process"
        );
        assert_eq!(
            startup_banner(&settings, 6, &["Hash".to_string()]),
            "lc0 at Depth 12 in 6 processes"
        );
        assert_eq!(
            failed_puzzle_line(&puzzle, "d4d5", 12),
            "00008 | r6k/pp2r2p/4Rp1Q/3p4/8/1N1P2R1/PqP2bPP/7K b - - 0 24 | f2g3 e6e7 b2b1 |  784 | played d4d5 | https://lichess.org/787zsVup/black#48"
        );
    }

    #[test]
    fn asks_for_the_depth_the_solution_needs() {
        let puzzle = parse_puzzle(PUZZLE_LINE).unwrap();

        assert_eq!(puzzle.solution.len(), 2);

        let shallow = failed_puzzle_line(&puzzle, "d4d5", 1);
        let deep = failed_puzzle_line(&puzzle, "d4d5", 2);

        assert_eq!(shallow, format!("{deep} | needs depth 2"));
    }

    #[cfg(unix)]
    mod shell_engine {
        use super::*;

        fn script(name: &str, answers: &str) -> String {
            let path = temp_path(name, "sh");
            fs::write(&path, answers).unwrap();

            format!("sh {}", path.display())
        }

        fn engine_script(cases: &str) -> String {
            format!(
                "while read -r command; do\n\
                 case \"$command\" in\n\
                 {cases}\n\
                 isready) echo readyok ;;\n\
                 quit) exit 0 ;;\n\
                 esac\n\
                 done\n"
            )
        }

        fn engine(name: &str, cases: &str) -> String {
            script(name, &engine_script(cases))
        }

        const ANSWERING_CASES: &str = r#"uci) echo "option name Hash type spin default 16 min 1 max 1024"; echo uciok ;;
"go depth 1") echo "info depth 0 score $MATE"; echo "bestmove (none)" ;;
go*) echo "info depth 16 score cp 31"; echo "bestmove e2e4 ponder e7e5" ;;"#;

        const LOGGING_CASES: &str = r#"uci) echo "option name Hash type spin default 16 min 1 max 1024"; echo uciok ;;
position*) echo "$command" >> "$LOG" ;;
go*) echo "bestmove e6e7" ;;"#;

        const OPTION_ANNOUNCING_CASES: &str = r#"uci)
    echo "id name Mock 1.0"
    echo "option name Hash type spin default 16 min 1 max 1024"
    echo "option name Threads type spin default 1 min 1 max 512"
    echo "option name Debug Log File type string default"
    echo uciok ;;
setoption*) echo "$command" >> "$LOG" ;;"#;

        const NO_OPTION_CASES: &str = "uci) echo uciok ;;";

        const ARGUMENT_GUARD: &str = r#"[ "$1" = "--uci" ] || exit 1
"#;

        const HANGING_ENGINE: &str = r#"echo $$ > "$PID"
echo uciok
read -r ignored
"#;

        fn answering(name: &str, score: &str) -> String {
            engine(name, &ANSWERING_CASES.replace("$MATE", score))
        }

        fn logging(name: &str, cases: &str) -> (String, PathBuf) {
            let path = temp_path(name, "log");
            let _ = fs::remove_file(&path);

            (
                engine(name, &cases.replace("$LOG", &path.display().to_string())),
                path,
            )
        }

        fn option_log(name: &str) -> (String, PathBuf) {
            logging(name, OPTION_ANNOUNCING_CASES)
        }

        #[test]
        fn sets_the_options_the_engine_announces() {
            let (command, log) = option_log("setoption");
            let options = [
                demanded("Threads", "4"),
                demanded("Hash", "256"),
                demanded("Debug Log File", "engine.log"),
            ];

            Engine::start(&command, &options).unwrap().quit().unwrap();

            assert_eq!(
                fs::read_to_string(&log).unwrap(),
                "setoption name Threads value 4\n\
             setoption name Hash value 256\n\
             setoption name Debug Log File value engine.log\n"
            );
        }

        #[test]
        fn fails_when_the_engine_does_not_support_an_option() {
            let (command, log) = option_log("unsupported-option");
            let options = [demanded("Ponder", "true")];

            let error = Engine::start(&command, &options).err().unwrap();

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error
                    .to_string()
                    .contains("engine does not support option `Ponder`")
            );
            assert!(!log.exists());
        }

        #[test]
        fn passes_arguments_to_the_engine() {
            let command = script(
                "arguments",
                &format!("{ARGUMENT_GUARD}{}", engine_script(NO_OPTION_CASES)),
            );

            Engine::start(&format!("{command} --uci"), &[])
                .unwrap()
                .quit()
                .unwrap();
            assert!(Engine::start(&command, &[]).is_err());
        }

        #[test]
        fn takes_the_move_from_the_bestmove_answer() {
            let mut engine = Engine::start(&answering("bestmove", "cp 0"), &[]).unwrap();

            let played = engine
                .best_move("8/8/8/8/8/8/8/K6k w - - 0 1", &["a1a2".to_string()], 16)
                .unwrap();

            assert_eq!(played, "e2e4");
            engine.quit().unwrap();
        }

        #[test]
        fn tells_checkmate_from_stalemate() {
            let position = "8/8/8/8/8/8/8/K6k w - - 0 1";

            let mut mating = Engine::start(&answering("mate", "mate 0"), &[]).unwrap();
            let mut stalling = Engine::start(&answering("stalemate", "cp 0"), &[]).unwrap();

            assert!(
                mating
                    .is_checkmate(position, &["a1a2".to_string()])
                    .unwrap()
            );
            assert!(
                !stalling
                    .is_checkmate(position, &["a1a2".to_string()])
                    .unwrap()
            );

            mating.quit().unwrap();
            stalling.quit().unwrap();
        }

        #[test]
        fn tests_every_puzzle_once_across_workers() {
            let rows: Vec<String> = ["a1a1", "b2b2", "c3c3"]
                .iter()
                .map(|first_move| PUZZLE_LINE.replace("f2g3", first_move))
                .collect();
            let mut settings =
                database("workers", &format!("{HEADER_LINE}\n{}\n", rows.join("\n")));
            settings.puzzle_count = 3;
            let (command, log) = logging("workers", LOGGING_CASES);
            settings.engine = command;
            let checked = Engine::start(&settings.engine, &settings.engine_options).unwrap();

            let run = Run::new(&settings, PuzzleSource::open(&settings).unwrap());
            run.solve(8, checked).unwrap();
            run.finish().unwrap();

            let mut searched: Vec<String> = fs::read_to_string(&log)
                .unwrap()
                .lines()
                .filter_map(|line| line.split_whitespace().last().map(str::to_string))
                .collect();
            searched.sort();
            assert_eq!(searched, ["a1a1", "b2b2", "c3c3"]);
        }

        #[test]
        fn fails_when_the_database_runs_out_of_puzzles() {
            let mut settings = database(
                "short-database",
                &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n"),
            );
            settings.puzzle_count = 5;
            settings.engine = answering("short-database", "cp 0");

            let error = solve_puzzles(&settings).err().unwrap();

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("read 2 puzzles, expected 5"));
        }

        #[test]
        fn fails_when_a_worker_cannot_start_its_engine() {
            let mut settings = database(
                "worker-engine",
                &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n"),
            );
            let checked = Engine::start(
                &answering("worker-engine", "cp 0"),
                &settings.engine_options,
            )
            .unwrap();
            settings.engine = "no-such-engine".to_string();

            let source = PuzzleSource::open(&settings).unwrap();
            let run = Run::new(&settings, source);
            let error = run.solve(2, checked).err().unwrap();

            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert!(error.to_string().contains("no-such-engine"));
        }

        #[test]
        fn checks_the_options_before_the_workers_start() {
            let (command, log) = option_log("checked-before-workers");
            let settings = Settings {
                engine: command,
                engine_options: vec![demanded("Ponder", "true")],
                ..database(
                    "checked-before-workers",
                    &format!("{HEADER_LINE}\n{PUZZLE_LINE}\n{PUZZLE_LINE}\n"),
                )
            };

            let error = solve_puzzles(&settings).err().unwrap();

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error
                    .to_string()
                    .contains("engine does not support option `Ponder`")
            );
            assert!(!log.exists());
        }

        #[test]
        fn refuses_a_bestmove_that_is_not_a_move() {
            let mut engine = Engine::start(&answering("none-bestmove", "cp 0"), &[]).unwrap();

            let error = engine
                .best_move("8/8/8/8/8/8/8/K6k w - - 0 1", &["a1a2".to_string()], 1)
                .err()
                .unwrap();

            assert!(
                error
                    .to_string()
                    .contains("engine answered `bestmove (none)`")
            );
            engine.quit().unwrap();
        }

        #[test]
        fn skips_a_default_the_engine_does_not_announce() {
            let command = engine("no-options", NO_OPTION_CASES);

            let engine = Engine::start(&command, &[defaulted("Hash", "16")]).unwrap();

            assert_eq!(engine.skipped_options, ["Hash"]);
            engine.quit().unwrap();
            assert!(Engine::start(&command, &[demanded("Hash", "256")]).is_err());
        }

        #[test]
        fn reaps_the_engine_it_could_not_configure() {
            let pid_path = temp_path("reaped", "pid");
            let _ = fs::remove_file(&pid_path);
            let command = script(
                "reaped",
                &HANGING_ENGINE.replace("$PID", &pid_path.display().to_string()),
            );

            let error = Engine::start(&command, &[demanded("Hash", "16")])
                .err()
                .unwrap();

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            let pid = fs::read_to_string(&pid_path).unwrap();
            let alive = Command::new("sh")
                .args(["-c", &format!("kill -0 {}", pid.trim())])
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(!alive.success());
        }

        #[test]
        fn fails_when_engine_stops_before_answering() {
            let silent = script("silent", "exit 0\n");

            let error = Engine::start(&silent, &[]).err().unwrap();

            assert!(error.to_string().contains("engine stopped before `uciok`"));
        }
    }
}
