use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_ENGINE: &str = "stockfish";
const DEFAULT_DATABASE_PATH: &str = "data/lichess_db_puzzle.csv";
const DEFAULT_SEARCH_DEPTH: u8 = 16;
const DEFAULT_PUZZLE_COUNT: usize = 100;

struct Puzzle {
    id: String,
    fen: String,
    opponent_move: String,
    solution: Vec<String>,
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

    for puzzle in &puzzles {
        println!(
            "{} | {:>4} | {} | {} | {}",
            puzzle.id,
            puzzle.rating,
            puzzle.fen,
            puzzle.opponent_move,
            puzzle.solution.join(" ")
        );
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::parse_puzzle;

    const PUZZLE_LINE: &str = "00008,r6k/pp2r2p/4Rp1Q/3p4/8/1N1P2R1/PqP2bPP/7K b - - 0 24,f2g3 e6e7 b2b1,1784,77,95,9822,crushing long,https://lichess.org/787zsVup/black#48,";

    #[test]
    fn splits_opponent_move_from_solution() {
        let puzzle = parse_puzzle(PUZZLE_LINE).unwrap();

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
}
