# sesame

A terminal UI for browsing AWS Secrets Manager. You can fuzzy-find a secret, open its value in a read-only viewer with vim keys, and copy exactly what you need to the clipboard.

## Features

- **Fuzzy search** across all secret names, with fzf-style syntax: `foo bar` (AND), `'exact`, `^prefix`, `suffix$`, `!exclude`. Search is smart-case.
- **Streaming list**: secrets appear as pages arrive. If listing fails partway through, the title marks the list as incomplete.
- **Value viewer**: JSON secrets are pretty-printed. You can move and copy with vim motions, text objects, visual mode and `/` search.
- **Exact copies**: when you copy part of a JSON string value (`yiq`, `yiw`, `v…y`), the JSON escapes are removed, so you get the real value, not the escaped form.
- **Details view** shows the ARN, KMS key, dates, rotation and tags.

## Install

```sh
brew install twaslowski/tap/sesame
```

Or download an archive from the [releases](https://github.com/twaslowski/sesame/releases), or build from source with `cargo install --path .`.

## Releasing

Bump `version` in `Cargo.toml`, commit and push, then tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The tag runs `.github/workflows/release.yml`. It checks the tag against `Cargo.toml`, builds macOS and Linux binaries
(arm64 and x86_64), and publishes the GitHub release. It then updates the cask in
[twaslowski/homebrew-tap](https://github.com/twaslowski/homebrew-tap) with the `HOMEBREW_TAP_DEPLOY_KEY` secret,
a write deploy key on the tap.

## Usage

`sesame` uses the standard AWS credential chain, so choose a profile and region in the usual way:

```sh
AWS_PROFILE=my-profile AWS_REGION=eu-central-1 sesame
```

### Secret list

| Key | Action |
|-----|--------|
| type | fuzzy search (the app starts in search mode) |
| `↑`/`↓`, `^n`/`^p` | move the selection |
| `^w` / `^u` | delete the last word / the whole query |
| `Esc` | go to normal mode (or clear the query when already in normal mode) |
| `⏎` | open the value in the viewer |
| `j`/`k`, `gg`/`G`, `^d`/`^u`, `^f`/`^b` | move the selection |
| `/`, `i` | go back to search mode |
| `d` | show the details |
| `y` / `Y` / `A` | copy the value / the name / the ARN |
| `q`, `^c` | quit |

### Viewer

| Key | Action |
|-----|--------|
| `hjkl w b e W B E 0 ^ $ gg G ^d ^u` | motions; you can give a count |
| `f t F T ; ,` | find a character on the line |
| `/`, `n`, `N` | search |
| `y{motion}`, `yy`, `Y` | copy |
| `yiw yaw yiq yi" yi{ yi[ yib …` | copy a text object (`q` = any quote) |
| `v`, `V`, `o` | visual mode / visual line mode / jump to the other end of the selection |
| `q`, `Esc` | close the viewer |

## Limitations

- Binary secrets are not shown or copied.
- Duplicate keys in a JSON secret collapse to the last value in the viewer.
- Wide characters (CJK, emoji) can put the cursor in the wrong column.
