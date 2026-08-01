# Chess puzzle solver

A command-line tool for testing chess engines by puzzles.

## Build

```sh
cargo build --release
```

The binary is named `cps` and lands in `target/release/`.
Requires Rust 1.85 or newer.

## Puzzle database

Puzzles come from the [Lichess open database](https://database.lichess.org/),
distributed as a Zstandard-compressed CSV in the Lichess puzzle format. Download
and unpack it into `data/`:

```sh
mkdir -p data
curl -L https://database.lichess.org/lichess_db_puzzle.csv.zst \
  | zstd -d -o data/lichess_db_puzzle.csv
```

The archive is roughly 300 MB and expands to about 1 GB, holding more than 6
million puzzles. Each row has the following columns:

```
PuzzleId,FEN,Moves,Rating,RatingDeviation,Popularity,NbPlays,Themes,GameUrl,OpeningTags
```

`FEN` is the position before the opponent moves, and the first move in `Moves`
is that opponent move. Apply it to the FEN to get the position the solver is
asked about — the solution starts from the second move.

## Usage

```sh
cargo run --release
```

Every flag is optional and falls back to its default:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--engine "COMMAND [ARGUMENTS]"` | `stockfish` | UCI engine to run, with arguments if it needs any |
| `--database PATH` | `data/lichess_db_puzzle.csv` | puzzle database to read |
| `--depth PLIES` | `16` | search depth per puzzle |
| `--count NUMBER` | `100` | how many puzzles to test |
| `--show-failed` | off | list unsolved puzzles after the run |

The engine gets `ucinewgame` before every puzzle, so puzzles do not warm the hash
for each other. A puzzle counts as solved when the engine plays the recorded move,
or when it plays a different move that is also checkmate.

```sh
cps --engine stockfish --depth 20 --count 500 --show-failed
```

Unsolved puzzles are listed as id, rating, the move the engine played, the moves
recorded in the database and the game link:

```
000VW | 2869 | played h5g5 | g6f5 d5c5 c2e4 h5g5 g7h8 g5f6 | https://lichess.org/e9AY2m5j/black#50
```
