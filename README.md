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
