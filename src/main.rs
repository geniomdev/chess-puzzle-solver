use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::time::Instant;

const DEFAULT_ENGINE: &str = "stockfish";
const DEFAULT_DATABASE_PATH: &str = "data/lichess_db_puzzle.csv";
const DEFAULT_SEARCH_DEPTH: u8 = 16;
const DEFAULT_PUZZLE_COUNT: usize = 100;

struct Puzzle {
    #[allow(dead_code)]
    id: String,
    fen: String,
    opponent_move: String,
    solution: Vec<String>,
    #[allow(dead_code)]
    rating: u32,
}

struct Settings {
    engine: String,
    database_path: PathBuf,
    search_depth: u8,
    puzzle_count: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: DEFAULT_ENGINE.to_string(),
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            search_depth: DEFAULT_SEARCH_DEPTH,
            puzzle_count: DEFAULT_PUZZLE_COUNT,
        }
    }
}

struct Engine {
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Engine {
    fn start(command: &str) -> io::Result<Self> {
        let mut process = Command::new(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| io::Error::new(error.kind(), format!("{command}: {error}")))?;

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
            moves.push(played);
            if engine.is_checkmate(&puzzle.fen, &moves)? {
                solved += 1;
            }
        }

        let tested = index + 1;
        print!(
            "\rsolved {solved} of {tested} ({:.1}%) in {:.1}s",
            100.0 * solved as f64 / tested as f64,
            started.elapsed().as_secs_f64()
        );
        io::stdout().flush()?;
    }
    println!();

    engine.quit()
}

fn main() -> ExitCode {
    let settings = Settings::default();
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
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("cps-{name}.sh"));
        fs::write(&path, answers).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        path.display().to_string()
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
