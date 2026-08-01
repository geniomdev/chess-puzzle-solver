use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::time::Instant;

const DEFAULT_ENGINE: &str = "stockfish";
const DEFAULT_DATABASE_PATH: &str = "data/lichess_db_puzzle.csv";
const DEFAULT_SEARCH_DEPTH: u8 = 16;
const DEFAULT_PUZZLE_COUNT: usize = 100;
const USAGE: &str = "usage: cps [--engine \"COMMAND [ARGUMENTS]\"] [--database PATH] [--depth PLIES] [--count NUMBER] [--show-failed]";

struct Puzzle {
    id: String,
    fen: String,
    opponent_move: String,
    solution: Vec<String>,
    rating: u32,
    url: String,
}

struct Settings {
    engine: String,
    database_path: PathBuf,
    search_depth: u8,
    puzzle_count: usize,
    show_failed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: DEFAULT_ENGINE.to_string(),
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            search_depth: DEFAULT_SEARCH_DEPTH,
            puzzle_count: DEFAULT_PUZZLE_COUNT,
            show_failed: false,
        }
    }
}

fn parse_settings(arguments: impl IntoIterator<Item = String>) -> io::Result<Settings> {
    fn value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
        arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{flag} needs a value\n{USAGE}"),
            )
        })
    }

    fn number<T: std::str::FromStr>(text: &str, flag: &str) -> io::Result<T> {
        text.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{flag} needs a number, not `{text}`\n{USAGE}"),
            )
        })
    }

    fn out_of_range(flag: &str, expectation: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} needs {expectation}\n{USAGE}"),
        )
    }

    let mut settings = Settings::default();
    let mut arguments = arguments.into_iter();

    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--engine" => {
                settings.engine = value(&mut arguments, &flag)?;
                if settings.engine.split_whitespace().next().is_none() {
                    return Err(out_of_range(&flag, "a command"));
                }
            }
            "--database" => settings.database_path = PathBuf::from(value(&mut arguments, &flag)?),
            "--depth" => {
                let plies: u32 = number(&value(&mut arguments, &flag)?, &flag)?;
                settings.search_depth = u8::try_from(plies)
                    .map_err(|_| out_of_range(&flag, "a depth between 1 and 255 plies"))?;
            }
            "--count" => settings.puzzle_count = number(&value(&mut arguments, &flag)?, &flag)?,
            "--show-failed" => settings.show_failed = true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown flag `{flag}`\n{USAGE}"),
                ));
            }
        }
    }

    if settings.search_depth == 0 {
        return Err(out_of_range("--depth", "a depth between 1 and 255 plies"));
    }
    if settings.puzzle_count == 0 {
        return Err(out_of_range("--count", "at least one puzzle"));
    }

    Ok(settings)
}

struct Engine {
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Engine {
    fn start(command: &str) -> io::Result<Self> {
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

        let input = process
            .stdin
            .take()
            .ok_or_else(|| io::Error::other(format!("{command}: stdin is not available")))?;
        let output = process
            .stdout
            .take()
            .ok_or_else(|| io::Error::other(format!("{command}: stdout is not available")))?;

        let mut engine = Self {
            process,
            input,
            output: BufReader::new(output),
        };
        engine.send("uci")?;
        engine.wait_for("uciok")?;
        engine.send("isready")?;
        engine.wait_for("readyok")?;

        Ok(engine)
    }

    fn send(&mut self, command: &str) -> io::Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()
    }

    fn wait_for(&mut self, token: &str) -> io::Result<String> {
        loop {
            let line = self.read_line(token)?;
            if line.split_whitespace().next() == Some(token) {
                return Ok(line);
            }
        }
    }

    fn read_line(&mut self, awaited: &str) -> io::Result<String> {
        let mut line = String::new();
        if self.output.read_line(&mut line)? == 0 {
            return Err(io::Error::other(format!(
                "engine stopped before `{awaited}`"
            )));
        }
        Ok(line)
    }

    fn best_move(&mut self, fen: &str, moves: &[String], depth: u8) -> io::Result<String> {
        self.send(&format!("position fen {fen} moves {}", moves.join(" ")))?;
        self.send(&format!("go depth {depth}"))?;

        let answer = self.wait_for("bestmove")?;
        answer
            .split_whitespace()
            .nth(1)
            .map(str::to_string)
            .ok_or_else(|| io::Error::other(format!("engine answered `{}`", answer.trim())))
    }

    fn is_checkmate(&mut self, fen: &str, moves: &[String]) -> io::Result<bool> {
        self.send(&format!("position fen {fen} moves {}", moves.join(" ")))?;
        self.send("go depth 1")?;

        let mut mate_score = false;
        loop {
            let line = self.read_line("bestmove")?;
            mate_score |= line.contains("score mate 0");
            if line.split_whitespace().next() == Some("bestmove") {
                return Ok(mate_score && line.contains("(none)"));
            }
        }
    }

    fn quit(mut self) -> io::Result<()> {
        self.send("quit")?;
        self.process.wait().map(|_| ())
    }
}

fn parse_puzzle(line: &str) -> Option<Puzzle> {
    let mut fields = line.split(',');
    let id = fields.next()?;
    let fen = fields.next()?;
    let mut moves = fields.next()?.split_whitespace();
    let rating = fields.next()?.parse().ok()?;

    let url = fields.nth(4).unwrap_or_default();

    let opponent_move = moves.next()?.to_string();
    let solution: Vec<String> = moves.map(str::to_string).collect();
    if solution.is_empty() {
        return None;
    }

    Some(Puzzle {
        id: id.to_string(),
        fen: fen.to_string(),
        opponent_move,
        solution,
        rating,
        url: url.to_string(),
    })
}

fn read_puzzles(settings: &Settings) -> io::Result<Vec<Puzzle>> {
    let path = &settings.database_path;
    let file = File::open(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;

    let mut puzzles = Vec::new();
    for line in BufReader::new(file).lines().skip(1) {
        if puzzles.len() == settings.puzzle_count {
            break;
        }
        if let Some(puzzle) = parse_puzzle(&line?) {
            puzzles.push(puzzle);
        }
    }

    if puzzles.len() < settings.puzzle_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: read {} puzzles, expected {}",
                path.display(),
                puzzles.len(),
                settings.puzzle_count
            ),
        ));
    }

    Ok(puzzles)
}

fn solve_puzzles(settings: &Settings, puzzles: &[Puzzle]) -> io::Result<()> {
    let mut engine = Engine::start(&settings.engine)?;
    let mut solved = 0;
    let mut failed = Vec::new();
    let started = Instant::now();

    for (index, puzzle) in puzzles.iter().enumerate() {
        engine.send("ucinewgame")?;
        engine.send("isready")?;
        engine.wait_for("readyok")?;

        let expected = &puzzle.solution[0];
        let mut moves = vec![puzzle.opponent_move.clone()];
        let played = engine.best_move(&puzzle.fen, &moves, settings.search_depth)?;

        if played == *expected {
            solved += 1;
        } else {
            moves.push(played.clone());
            if engine.is_checkmate(&puzzle.fen, &moves)? {
                solved += 1;
            } else {
                failed.push((puzzle, played));
            }
        }

        let tested = index + 1;
        let progress = format!(
            "solved {solved} of {tested} ({:.1}%) in {:.1}s",
            100.0 * solved as f64 / tested as f64,
            started.elapsed().as_secs_f64()
        );
        print!("\r{progress:<48}");
        io::stdout().flush()?;
    }
    println!();

    if settings.show_failed {
        for (puzzle, played) in failed {
            println!(
                "{} | {:>4} | played {played} | {} {} | {}",
                puzzle.id,
                puzzle.rating,
                puzzle.opponent_move,
                puzzle.solution.join(" "),
                puzzle.url
            );
        }
    }

    engine.quit()
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let settings = match parse_settings(arguments) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("cps: {error}");
            return ExitCode::FAILURE;
        }
    };

    let puzzles = match read_puzzles(&settings) {
        Ok(puzzles) => puzzles,
        Err(error) => {
            eprintln!("cps: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("{} at depth {}", settings.engine, settings.search_depth);
    println!(
        "read {} puzzles from {}",
        puzzles.len(),
        settings.database_path.display()
    );

    if let Err(error) = solve_puzzles(&settings, &puzzles) {
        eprintln!("cps: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
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

    fn database(name: &str, contents: &str) -> Settings {
        let path = std::env::temp_dir().join(format!("cps-{name}.csv"));
        fs::write(&path, contents).unwrap();

        Settings {
            database_path: path,
            puzzle_count: 2,
            ..Settings::default()
        }
    }

    #[cfg(unix)]
    fn script(name: &str, answers: &str) -> String {
        let path = std::env::temp_dir().join(format!("cps-{name}.sh"));
        fs::write(&path, answers).unwrap();

        format!("sh {}", path.display())
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
    fn keeps_defaults_without_flags() {
        let settings = flags(&[]).unwrap();

        assert_eq!(settings.engine, "stockfish");
        assert_eq!(settings.database_path, PathBuf::from(DEFAULT_DATABASE_PATH));
        assert_eq!(settings.search_depth, 16);
        assert_eq!(settings.puzzle_count, 100);
        assert!(!settings.show_failed);
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
            "--show-failed",
        ])
        .unwrap();

        assert_eq!(settings.engine, "lc0");
        assert_eq!(settings.database_path, PathBuf::from("puzzles.csv"));
        assert_eq!(settings.search_depth, 8);
        assert_eq!(settings.puzzle_count, 3);
        assert!(settings.show_failed);
    }

    #[test]
    fn rejects_unknown_flag() {
        let error = flags(&["--wat"]).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("unknown flag `--wat`"));
    }

    #[test]
    fn rejects_flag_without_value() {
        let error = flags(&["--depth"]).err().unwrap();

        assert!(error.to_string().contains("--depth needs a value"));
    }

    #[test]
    fn rejects_depth_outside_the_uci_range() {
        for depth in ["0", "256"] {
            let error = flags(&["--depth", depth]).err().unwrap();

            assert!(
                error
                    .to_string()
                    .contains("--depth needs a depth between 1 and 255 plies")
            );
        }
    }

    #[test]
    fn rejects_empty_run() {
        let error = flags(&["--count", "0"]).err().unwrap();

        assert!(
            error
                .to_string()
                .contains("--count needs at least one puzzle")
        );
    }

    #[test]
    fn rejects_value_that_is_not_a_number() {
        let error = flags(&["--count", "many"]).err().unwrap();

        assert!(
            error
                .to_string()
                .contains("--count needs a number, not `many`")
        );
    }

    #[test]
    fn rejects_header() {
        assert!(parse_puzzle("PuzzleId,FEN,Moves,Rating").is_none());
    }

    #[test]
    fn rejects_line_without_solution() {
        assert!(parse_puzzle("00008,8/8/8/8/8/8/8/K6k w - - 0 1,f2g3,1784").is_none());
    }

    #[test]
    fn rejects_empty_line() {
        assert!(parse_puzzle("").is_none());
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
        let error = Engine::start("no-such-engine").err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no-such-engine"));
    }

    #[cfg(unix)]
    const ANSWERING_ENGINE: &str = r#"#!/bin/sh
while read -r command; do
    case "$command" in
    uci) echo uciok ;;
    isready) echo readyok ;;
    "go depth 1") echo "info depth 0 score $MATE"; echo "bestmove (none)" ;;
    go*) echo "info depth 16 score cp 31"; echo "bestmove e2e4 ponder e7e5" ;;
    quit) exit 0 ;;
    esac
done
"#;

    #[cfg(unix)]
    const ENGINE_NEEDING_ARGUMENTS: &str = r#"#!/bin/sh
[ "$1" = "--uci" ] || exit 1
while read -r command; do
    case "$command" in
    uci) echo uciok ;;
    isready) echo readyok ;;
    quit) exit 0 ;;
    esac
done
"#;

    #[cfg(unix)]
    #[test]
    fn passes_arguments_to_the_engine() {
        let command = script("arguments", ENGINE_NEEDING_ARGUMENTS);

        Engine::start(&format!("{command} --uci"))
            .unwrap()
            .quit()
            .unwrap();
        assert!(Engine::start(&command).is_err());
    }

    #[test]
    fn rejects_empty_engine_command() {
        for error in [
            flags(&["--engine", "   "]).err().unwrap(),
            Engine::start("   ").err().unwrap(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("--engine needs a command"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn takes_the_move_from_the_bestmove_answer() {
        let mut engine = Engine::start(&script("bestmove", ANSWERING_ENGINE)).unwrap();

        let played = engine
            .best_move("8/8/8/8/8/8/8/K6k w - - 0 1", &["a1a2".to_string()], 16)
            .unwrap();

        assert_eq!(played, "e2e4");
        engine.quit().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tells_checkmate_from_stalemate() {
        let mate = ANSWERING_ENGINE.replace("$MATE", "mate 0");
        let stalemate = ANSWERING_ENGINE.replace("$MATE", "cp 0");
        let position = "8/8/8/8/8/8/8/K6k w - - 0 1";

        let mut mating = Engine::start(&script("mate", &mate)).unwrap();
        let mut stalling = Engine::start(&script("stalemate", &stalemate)).unwrap();

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

    #[cfg(unix)]
    #[test]
    fn fails_when_engine_stops_before_answering() {
        let silent = script("silent", "#!/bin/sh\nexit 0\n");

        let error = Engine::start(&silent).err().unwrap();

        assert!(error.to_string().contains("engine stopped before `uciok`"));
    }
}
