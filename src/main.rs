use aws_sdk_secretsmanager::{
    Client,
    error::DisplayErrorContext,
    primitives::{DateTime, DateTimeFormat},
    types::SecretListEntry,
};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    prelude::*,
    widgets::*,
};
use std::time::Duration;
use tokio::sync::mpsc;

const POPUP_BG: Color = Color::Indexed(235);

enum Msg {
    Page(Vec<SecretListEntry>),
    Done,
    Err(String),
}

struct App {
    client: Client,
    secrets: Vec<SecretListEntry>,
    filtered: Vec<usize>,
    list: ListState,
    query: String,
    searching: bool,
    popup: Option<Viewer>,
    pending_g: bool,
    loading: bool,
    list_failed: bool, // shown in the list title so a partial list never looks complete
    status: String,
    page_height: usize,
    clipboard: Option<arboard::Clipboard>, // kept alive: on X11 the selection dies with its owner
    quit: bool,
}

#[tokio::main]
async fn main() {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = Client::new(&config);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let pager = client.clone();
    tokio::spawn(async move {
        let mut pages = pager
            .list_secrets()
            .max_results(100)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            match page {
                Ok(p) => _ = tx.send(Msg::Page(p.secret_list().to_vec())),
                Err(e) => {
                    _ = tx.send(Msg::Err(DisplayErrorContext(e).to_string()));
                    return;
                }
            }
        }
        _ = tx.send(Msg::Done);
    });

    let mut app = App {
        client,
        secrets: vec![],
        filtered: vec![],
        list: ListState::default(),
        query: String::new(),
        searching: true,
        popup: None,
        pending_g: false,
        loading: true,
        list_failed: false,
        status: String::new(),
        page_height: 10,
        clipboard: arboard::Clipboard::new().ok(),
        quit: false,
    };

    let mut term = ratatui::init();
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Page(p) => {
                    app.secrets.extend(p);
                    app.refilter();
                }
                Msg::Done => app.loading = false,
                Msg::Err(e) => {
                    app.loading = false;
                    app.list_failed = true;
                    app.status = format!("list failed: {e}");
                }
            }
        }
        if let Err(e) = term.draw(|f| app.draw(f)) {
            ratatui::restore();
            panic!("{e}");
        }
        if event::poll(Duration::from_millis(100)).unwrap_or(false)
            && let Ok(Event::Key(k)) = event::read()
            && k.kind == KeyEventKind::Press
        {
            app.on_key(k).await;
        }
    }
    ratatui::restore();
}

impl App {
    fn selected(&self) -> Option<&SecretListEntry> {
        self.list
            .selected()
            .and_then(|i| self.filtered.get(i))
            .map(|&i| &self.secrets[i])
    }

    /// Re-filter while keeping the selected secret (used when new pages arrive).
    fn refilter(&mut self) {
        let keep = self.selected().and_then(|s| s.arn.clone());
        self.filtered = filter(&self.secrets, &self.query);
        let idx = keep
            .and_then(|arn| {
                self.filtered
                    .iter()
                    .position(|&i| self.secrets[i].arn.as_deref() == Some(&arn))
            })
            .unwrap_or(0);
        self.list.select((!self.filtered.is_empty()).then_some(idx));
    }

    /// Re-filter after the query changed: jump to the best match.
    fn requery(&mut self) {
        self.filtered = filter(&self.secrets, &self.query);
        self.list.select((!self.filtered.is_empty()).then_some(0));
    }

    fn move_by(&mut self, d: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        let cur = self.list.selected().unwrap_or(0) as isize;
        self.list.select(Some((cur + d).clamp(0, max) as usize));
    }

    async fn fetch_value(&mut self) -> Option<String> {
        let id = self.selected()?.arn.clone()?;
        match self.client.get_secret_value().secret_id(id).send().await {
            Ok(o) => {
                if let Some(s) = o.secret_string() {
                    return Some(s.into());
                }
                self.status = match o.secret_binary() {
                    Some(b) => format!("binary secret ({} bytes), not shown", b.as_ref().len()),
                    None => "secret has no value".into(),
                };
                None
            }
            Err(e) => {
                self.status = format!("get failed: {}", DisplayErrorContext(e));
                None
            }
        }
    }

    async fn open_value(&mut self) {
        if let Some(v) = self.fetch_value().await {
            let title = self
                .selected()
                .and_then(|s| s.name.clone())
                .unwrap_or_default();
            self.popup = Some(match pretty(&v) {
                Some(p) => Viewer {
                    json: true,
                    ..Viewer::new(title, &p)
                },
                None => Viewer::new(title, &v),
            });
        }
    }

    fn yank(&mut self, text: String, what: &str) {
        self.status = match self.clipboard.as_mut().map(|c| c.set_text(text)) {
            Some(Ok(())) => format!("yanked {what}"),
            Some(Err(e)) => format!("yank failed: {e}"),
            None => "no clipboard available".into(),
        };
    }

    async fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        if let Some(v) = &mut self.popup {
            self.status.clear();
            match v.on_key(k) {
                Some(Action::Close) => self.popup = None,
                Some(Action::Yank(text, what)) => self.yank(text, &what),
                None => {}
            }
            return;
        }

        if self.searching {
            match k.code {
                KeyCode::Esc => self.searching = false,
                KeyCode::Enter => self.open_value().await,
                KeyCode::Down => self.move_by(1),
                KeyCode::Up => self.move_by(-1),
                KeyCode::Char('n') if ctrl => self.move_by(1),
                KeyCode::Char('p') if ctrl => self.move_by(-1),
                KeyCode::Char('u') if ctrl => {
                    self.query.clear();
                    self.requery();
                }
                KeyCode::Char('w') if ctrl => {
                    let t = self.query.trim_end_matches(|c: char| !c.is_alphanumeric());
                    let n = t.trim_end_matches(char::is_alphanumeric).len();
                    self.query.truncate(n);
                    self.requery();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.requery();
                }
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    self.requery();
                }
                _ => {}
            }
            return;
        }

        let g = std::mem::take(&mut self.pending_g);
        let half = (self.page_height / 2).max(1) as isize;
        self.status.clear();
        match k.code {
            KeyCode::Char('d') if ctrl => self.move_by(half),
            KeyCode::Char('u') if ctrl => self.move_by(-half),
            KeyCode::Char('f') if ctrl => self.move_by(self.page_height as isize),
            KeyCode::Char('b') if ctrl => self.move_by(-(self.page_height as isize)),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') if g => self.move_by(isize::MIN / 2),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Char('/' | 'i') => self.searching = true,
            KeyCode::Esc => {
                self.query.clear();
                self.requery();
            }
            KeyCode::Enter => self.open_value().await,
            KeyCode::Char('d') => {
                if let Some(s) = self.selected() {
                    let title = format!("{} — details", s.name().unwrap_or_default());
                    self.popup = Some(Viewer::new(title, &describe(s)));
                }
            }
            KeyCode::Char('y') => {
                if let Some(v) = self.fetch_value().await {
                    self.yank(v, "secret value");
                }
            }
            KeyCode::Char('Y') => {
                if let Some(n) = self.selected().and_then(|s| s.name.clone()) {
                    self.yank(n, "name");
                }
            }
            KeyCode::Char('A') => {
                if let Some(a) = self.selected().and_then(|s| s.arn.clone()) {
                    self.yank(a, "ARN");
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [search, main, bar] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.page_height = main.height.saturating_sub(2) as usize;

        let input = if self.query.is_empty() && self.searching {
            Line::from("fuzzy search · space = AND · 'exact ^prefix suffix$ !exclude").dark_gray()
        } else {
            Line::from(self.query.as_str())
        };
        let search_block = Block::bordered()
            .title(" Search ")
            .border_style(if self.searching {
                Style::new().cyan()
            } else {
                Style::new()
            });
        f.render_widget(Paragraph::new(input).block(search_block), search);
        if self.searching && self.popup.is_none() {
            f.set_cursor_position((
                search.x + 1 + self.query.chars().count() as u16,
                search.y + 1,
            ));
        }

        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .map(|&i| {
                let s = &self.secrets[i];
                let mut spans = vec![Span::raw(s.name().unwrap_or_default().to_string())];
                if let Some(d) = s.description().filter(|d| !d.is_empty()) {
                    spans.push(Span::styled(format!("  {d}"), Style::new().dark_gray()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = format!(
            " Secrets {}/{}{} ",
            self.filtered.len(),
            self.secrets.len(),
            if self.list_failed {
                " (INCOMPLETE: list failed)"
            } else if self.loading {
                " (loading…)"
            } else {
                ""
            }
        );
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .style(if self.popup.is_some() {
                Style::new().dark_gray()
            } else {
                Style::new()
            })
            .highlight_style(Style::new().reversed())
            .highlight_symbol("> ");
        f.render_stateful_widget(list, main, &mut self.list);

        let bar_text = if !self.status.is_empty() {
            Line::from(self.status.as_str()).yellow()
        } else if self.popup.is_some() {
            Line::from("")
        } else if self.searching {
            Line::from(
                "⏎ value  ↑/↓ ^n/^p move  ^w/^u delete  Esc normal mode (d details, y yank, …)",
            )
            .dark_gray()
        } else {
            Line::from("j/k gg/G ^d/^u  / i search  ⏎ value  d details  y yank value  Y name  A arn  Esc clear  q quit")
                .dark_gray()
        };
        f.render_widget(bar_text, bar);

        if let Some(v) = &mut self.popup {
            v.draw(
                f,
                main.inner(Margin::new(main.width / 10, main.height / 10)),
            );
        }
    }
}

type Pos = (usize, usize);

const MAX_COUNT: usize = 99_999;

#[derive(Clone, Copy, PartialEq)]
enum Visual {
    Char,
    Line,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

enum Action {
    Close,
    Yank(String, String),
}

/// Read-only vim viewer: `[count][y]{motion}`, `y{text object}`, visual mode, `/` search.
/// Positions may sit on a virtual '\n' at col == line length, so motions and objects cross lines.
struct Viewer {
    title: String,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    want: usize, // column j/k try to return to
    top: usize,
    left: usize,
    height: usize,
    width: usize,
    visual: Option<(Visual, Pos)>, // kind, anchor
    search: Option<String>,        // Some while typing a /query
    last_search: Vec<char>,
    last_find: Option<(char, char)>, // (f/F/t/T, target) for ; and ,
    count: usize,
    op: bool,             // `y` pending
    prefix: Option<char>, // g, i/a, f/F/t/T awaiting their second key
    keys: String,         // showcmd
    msg: String,
    json: bool, // body is re-serialized JSON: charwise yanks get JSON-unescaped
}

impl Viewer {
    fn new(title: String, body: &str) -> Self {
        let lines: Vec<Vec<char>> = body
            .strip_suffix('\n')
            .unwrap_or(body)
            .split('\n')
            .map(|l| l.chars().collect())
            .collect();
        Viewer {
            title,
            lines,
            row: 0,
            col: 0,
            want: 0,
            top: 0,
            left: 0,
            height: 1,
            width: 1,
            visual: None,
            search: None,
            last_search: vec![],
            last_find: None,
            count: 0,
            op: false,
            prefix: None,
            keys: String::new(),
            msg: String::new(),
            json: false,
        }
    }

    fn at(&self, (r, c): Pos) -> char {
        self.lines[r].get(c).copied().unwrap_or('\n')
    }

    fn next(&self, (r, c): Pos) -> Option<Pos> {
        if c < self.lines[r].len() {
            Some((r, c + 1))
        } else if r + 1 < self.lines.len() {
            Some((r + 1, 0))
        } else {
            None
        }
    }

    fn prev(&self, (r, c): Pos) -> Option<Pos> {
        if c > 0 {
            Some((r, c - 1))
        } else if r > 0 {
            Some((r - 1, self.lines[r - 1].len()))
        } else {
            None
        }
    }

    /// Inclusive range as text; empty when `b < a` (e.g. `i"` on `""`).
    fn text(&self, a: Pos, b: Pos) -> String {
        let mut s = String::new();
        let mut p = a;
        while p <= b {
            s.push(self.at(p));
            match self.next(p) {
                Some(q) => p = q,
                None => break,
            }
        }
        s
    }

    fn line_text(&self, a: usize, b: usize) -> String {
        self.lines[a..=b]
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn first_non_blank(&self, r: usize) -> usize {
        self.lines[r]
            .iter()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0)
    }

    fn clamp(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len().saturating_sub(1));
    }

    fn pending(&self) -> bool {
        self.count > 0 || self.op || self.prefix.is_some()
    }

    fn reset(&mut self) {
        (self.count, self.op, self.prefix) = (0, false, None);
    }

    fn on_key(&mut self, k: KeyEvent) -> Option<Action> {
        if let Some(q) = &mut self.search {
            match k.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Enter => {
                    self.last_search = self.search.take().unwrap_or_default().chars().collect();
                    return self.feed('n');
                }
                KeyCode::Backspace => _ = q.pop(),
                KeyCode::Char(c) => q.push(c),
                _ => {}
            }
            return None;
        }

        self.msg.clear();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let c = match k.code {
            KeyCode::Char('d') if ctrl => '\x04',
            KeyCode::Char('u') if ctrl => '\x15',
            KeyCode::Char(c) if !ctrl => c,
            KeyCode::Down => 'j',
            KeyCode::Up => 'k',
            KeyCode::Left => 'h',
            KeyCode::Right => 'l',
            KeyCode::Home => '0',
            KeyCode::End => '$',
            KeyCode::Esc if self.pending() => {
                self.reset();
                self.keys.clear();
                return None;
            }
            KeyCode::Esc if self.visual.is_some() => {
                self.visual = None;
                return None;
            }
            KeyCode::Esc => return Some(Action::Close),
            _ => return None,
        };
        self.keys.push(c);
        let res = self.feed(c);
        if !self.pending() {
            self.keys.clear();
        }
        self.clamp();
        res
    }

    fn feed(&mut self, c: char) -> Option<Action> {
        if let Some(p) = self.prefix.take() {
            return match p {
                'g' if c == 'g' => self.do_motion('g'),
                'i' | 'a' => self.do_textobj(p == 'i', c),
                'f' | 'F' | 't' | 'T' => {
                    self.last_find = Some((p, c));
                    self.do_motion(';')
                }
                _ => {
                    self.reset();
                    None
                }
            };
        }
        match c {
            '0'..='9' if c != '0' || self.count > 0 => {
                self.count = (self.count * 10 + c.to_digit(10).unwrap_or(0) as usize).min(MAX_COUNT)
            }
            'y' if self.visual.is_some() => {
                let (kind, a, b) = self.sel_range()?;
                self.visual = None;
                self.reset();
                (self.row, self.col) = if kind == Visual::Line {
                    (a.0, self.col)
                } else {
                    a
                };
                return Some(match kind {
                    Visual::Line => yanked(self.line_text(a.0, b.0)),
                    Visual::Char => self.yank_chars(self.text(a, b)),
                });
            }
            'y' if self.op => {
                let end = (self.row + self.count.max(1) - 1).min(self.lines.len() - 1);
                self.reset();
                return Some(yanked(self.line_text(self.row, end)));
            }
            'y' => self.op = true,
            'Y' => {
                self.op = true;
                return self.do_motion('$');
            }
            'g' | 'f' | 'F' | 't' | 'T' => self.prefix = Some(c),
            'i' | 'a' if self.op || self.visual.is_some() => self.prefix = Some(c),
            'v' | 'V' if !self.op => {
                let kind = if c == 'v' { Visual::Char } else { Visual::Line };
                self.visual = match self.visual {
                    Some((k, _)) if k == kind => None,
                    Some((_, a)) => Some((kind, a)),
                    None => Some((kind, (self.row, self.col))),
                };
                self.reset();
            }
            'o' if self.visual.is_some() => {
                if let Some((k, a)) = self.visual {
                    self.visual = Some((k, (self.row, self.col)));
                    (self.row, self.col) = a;
                }
                self.reset();
            }
            '/' if !self.op => {
                self.reset();
                self.search = Some(String::new());
            }
            'q' if !self.op => return Some(Action::Close),
            _ => return self.do_motion(c),
        }
        None
    }

    /// One step of motion `m` from `p`. `g` stands for `gg`.
    fn motion(&self, m: char, (r, c): Pos) -> Option<(Pos, Kind)> {
        use Kind::*;
        let len = self.lines[r].len();
        let last = self.lines.len() - 1;
        let half = (self.height / 2).max(1);
        Some(match m {
            'h' => ((r, c.checked_sub(1)?), Exclusive),
            'l' if c < len => ((r, c + 1), Exclusive),
            'j' if r < last => ((r + 1, self.want), Linewise),
            'k' if r > 0 => ((r - 1, self.want), Linewise),
            '\x04' => (((r + half).min(last), self.want), Linewise),
            '\x15' => ((r.saturating_sub(half), self.want), Linewise),
            '0' => ((r, 0), Exclusive),
            '^' => ((r, self.first_non_blank(r)), Exclusive),
            '$' => ((r, len.saturating_sub(1)), Inclusive),
            'w' | 'W' => (self.word_fwd((r, c), m == 'W'), Exclusive),
            'e' | 'E' => (self.word_end((r, c), m == 'E'), Inclusive),
            'b' | 'B' => (self.word_back((r, c), m == 'B'), Exclusive),
            'g' => ((0, self.first_non_blank(0)), Linewise),
            'G' => ((last, self.first_non_blank(last)), Linewise),
            'n' | 'N' => (self.search_from((r, c), m == 'n')?, Exclusive),
            ';' | ',' => {
                let (k, ch) = self.last_find?;
                let k = if m == ',' {
                    match k {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    k
                };
                let l = &self.lines[r];
                let fwd = || (c + 1..l.len()).find(|&i| l[i] == ch);
                let back = || (0..c).rev().find(|&i| l[i] == ch);
                match k {
                    'f' => ((r, fwd()?), Inclusive),
                    't' => ((r, fwd()? - 1), Inclusive),
                    'F' => ((r, back()?), Exclusive),
                    _ => ((r, back()? + 1), Exclusive),
                }
            }
            _ => return None,
        })
    }

    fn do_motion(&mut self, m: char) -> Option<Action> {
        let start = (self.row, self.col);
        let (count, op) = (self.count, self.op);
        self.reset();
        let Some((mut p, mut kind)) = self.motion(m, start) else {
            if matches!(m, 'n' | 'N') && !self.last_search.is_empty() {
                self.msg = format!("not found: {}", self.last_search.iter().collect::<String>());
            }
            return None;
        };
        let mut reps = count.max(1) - 1;
        if matches!(m, 'n' | 'N') {
            reps %= (0..self.lines.len())
                .map(|r| self.hits(r).len())
                .sum::<usize>()
                .max(1); // n wraps around
        }
        for _ in 0..reps {
            match self.motion(m, p) {
                Some((q, _)) if q.0 != p.0 || q.1 != p.1 => p = q,
                _ => break,
            }
        }
        if matches!(m, 'g' | 'G') && count > 0 {
            p.0 = (count - 1).min(self.lines.len() - 1);
            p.1 = self.first_non_blank(p.0);
        }

        if !op {
            (self.row, self.col) = p;
            if !matches!(m, 'j' | 'k' | '\x04' | '\x15') {
                self.want = if m == '$' { usize::MAX } else { p.1 };
            }
            return None;
        }

        // vim: `yw` on a line's last word stops at the line end instead of spanning into the next line
        if matches!(m, 'w' | 'W') && p.0 > start.0 {
            (p, kind) = ((start.0, self.lines[start.0].len()), Kind::Exclusive);
        }
        let (a, b) = if p < start { (p, start) } else { (start, p) };
        let text = match kind {
            Kind::Linewise => self.line_text(a.0, b.0),
            Kind::Inclusive => self.text(a, b),
            Kind::Exclusive if a == b => String::new(),
            Kind::Exclusive => self.text(a, self.prev(b)?),
        };
        (self.row, self.col) = if kind == Kind::Linewise {
            (a.0, self.col)
        } else {
            a
        };
        Some(if kind == Kind::Linewise {
            yanked(text)
        } else {
            self.yank_chars(text)
        })
    }

    fn do_textobj(&mut self, inner: bool, c: char) -> Option<Action> {
        let op = self.op;
        self.reset();
        let (a, b) = match c {
            'w' | 'W' => self.word_obj(inner, c == 'W'),
            '"' | '\'' | '`' => self.quote_obj(inner, &[c]),
            'q' => self.quote_obj(inner, &['"', '\'', '`']),
            'b' | '(' | ')' => self.bracket_obj(inner, '(', ')'),
            'B' | '{' | '}' => self.bracket_obj(inner, '{', '}'),
            '[' | ']' => self.bracket_obj(inner, '[', ']'),
            '<' | '>' => self.bracket_obj(inner, '<', '>'),
            _ => None,
        }?;
        if op {
            (self.row, self.col) = a;
            return Some(self.yank_chars(self.text(a, b)));
        }
        self.visual = Some((Visual::Char, a));
        (self.row, self.col) = b.max(a);
        None
    }

    fn word_fwd(&self, mut p: Pos, big: bool) -> Pos {
        let k = class(self.at(p), big);
        while k != 0 && class(self.at(p), big) == k {
            let Some(q) = self.next(p) else { return p };
            p = q;
        }
        while class(self.at(p), big) == 0 {
            let Some(q) = self.next(p) else { return p };
            p = q;
        }
        p
    }

    fn word_end(&self, p: Pos, big: bool) -> Pos {
        let Some(mut p) = self.next(p) else { return p };
        while class(self.at(p), big) == 0 {
            let Some(q) = self.next(p) else { return p };
            p = q;
        }
        while let Some(q) = self
            .next(p)
            .filter(|&q| class(self.at(q), big) == class(self.at(p), big))
        {
            p = q;
        }
        p
    }

    fn word_back(&self, p: Pos, big: bool) -> Pos {
        let Some(mut p) = self.prev(p) else { return p };
        while class(self.at(p), big) == 0 {
            let Some(q) = self.prev(p) else { return p };
            p = q;
        }
        while let Some(q) = self
            .prev(p)
            .filter(|&q| class(self.at(q), big) == class(self.at(p), big))
        {
            p = q;
        }
        p
    }

    fn word_obj(&self, inner: bool, big: bool) -> Option<(Pos, Pos)> {
        let (r, l) = (self.row, &self.lines[self.row]);
        if l.is_empty() {
            return None;
        }
        let cls = |i: usize| class(l[i], big);
        let (mut s, mut e) = (self.col, self.col);
        while s > 0 && cls(s - 1) == cls(self.col) {
            s -= 1;
        }
        while e + 1 < l.len() && cls(e + 1) == cls(self.col) {
            e += 1;
        }
        if !inner {
            // aw: take trailing whitespace (or the following word when on whitespace), else leading whitespace
            if e + 1 < l.len() && (cls(e + 1) == 0 || cls(self.col) == 0) {
                let k = cls(e + 1);
                while e + 1 < l.len() && cls(e + 1) == k {
                    e += 1;
                }
            } else {
                while s > 0 && cls(s - 1) == 0 {
                    s -= 1;
                }
            }
        }
        Some(((r, s), (r, e)))
    }

    /// Quote pair containing the cursor, else the next one on the line, else the previous one
    /// (so `$yiq` grabs a JSON value).
    fn quote_obj(&self, inner: bool, quotes: &[char]) -> Option<(Pos, Pos)> {
        let (r, c, l) = (self.row, self.col, &self.lines[self.row]);
        let escaped = |i: usize| l[..i].iter().rev().take_while(|&&x| x == '\\').count() % 2 == 1;
        let mut pairs = vec![];
        for &q in quotes {
            let idx: Vec<usize> = (0..l.len()).filter(|&i| l[i] == q && !escaped(i)).collect();
            pairs.extend(idx.as_chunks::<2>().0.iter().map(|&[o, c]| (o, c)));
        }
        let &(o, cl) = pairs
            .iter()
            .filter(|p| p.0 <= c && c <= p.1)
            .min_by_key(|p| p.1 - p.0)
            .or_else(|| pairs.iter().filter(|p| p.0 > c).min_by_key(|p| p.0))
            .or_else(|| pairs.iter().filter(|p| p.1 < c).max_by_key(|p| p.1))?;
        Some(if inner {
            ((r, o + 1), (r, cl - 1))
        } else {
            ((r, o), (r, cl))
        })
    }

    fn bracket_obj(&self, inner: bool, open: char, close: char) -> Option<(Pos, Pos)> {
        let cur = (self.row, self.col);
        let (mut depth, mut p) = (0, cur);
        let o = loop {
            match self.at(p) {
                ch if ch == close && p != cur => depth += 1,
                ch if ch == open && depth == 0 => break p,
                ch if ch == open => depth -= 1,
                _ => {}
            }
            p = self.prev(p)?;
        };
        let (mut depth, mut p) = (0, o);
        let cl = loop {
            p = self.next(p)?;
            match self.at(p) {
                ch if ch == open => depth += 1,
                ch if ch == close && depth == 0 => break p,
                ch if ch == close => depth -= 1,
                _ => {}
            }
        };
        if !inner {
            return Some((o, cl));
        }
        // like vim, `i{` on a multi-line block drops the line breaks right after `{` and before `}`
        let mut a = self.next(o)?;
        if self.at(a) == '\n' && a < cl {
            a = self.next(a)?;
        }
        let mut b = self.prev(cl)?;
        if cl.0 > o.0 && self.lines[cl.0][..cl.1].iter().all(|c| c.is_whitespace()) {
            b = self.prev((cl.0, 0))?;
            if self.at(b) == '\n' && b.1 > 0 {
                b = self.prev(b)?;
            }
        }
        Some((a, b))
    }

    fn hits(&self, r: usize) -> Vec<(usize, usize)> {
        let (l, q) = (&self.lines[r], &self.last_search);
        if q.is_empty() || q.len() > l.len() {
            return vec![];
        }
        (0..=l.len() - q.len())
            .filter(|&i| l[i..i + q.len()] == q[..])
            .map(|i| (i, i + q.len()))
            .collect()
    }

    fn search_from(&self, cur: Pos, fwd: bool) -> Option<Pos> {
        let hits: Vec<Pos> = (0..self.lines.len())
            .flat_map(|r| self.hits(r).into_iter().map(move |(c, _)| (r, c)))
            .collect();
        let hit = if fwd {
            hits.iter().find(|&&h| h > cur).or(hits.first())
        } else {
            hits.iter().rev().find(|&&h| h < cur).or(hits.last())
        };
        hit.copied()
    }

    /// Ordered visual range: (kind, start, end), both ends inclusive.
    fn sel_range(&self) -> Option<(Visual, Pos, Pos)> {
        let (kind, a) = self.visual?;
        let b = (self.row, self.col);
        Some(if a <= b { (kind, a, b) } else { (kind, b, a) })
    }

    fn is_selected(&self, r: usize, c: usize) -> bool {
        match self.sel_range() {
            Some((Visual::Line, a, b)) => (a.0..=b.0).contains(&r),
            Some((Visual::Char, a, b)) => (a..=b).contains(&(r, c)),
            None => false,
        }
    }

    fn footer(&self) -> String {
        let right = format!("{}  {}:{}", self.keys, self.row + 1, self.col + 1);
        let left = if let Some(q) = &self.search {
            return format!(" /{q} ");
        } else if !self.msg.is_empty() {
            self.msg.clone()
        } else if let Some((kind, _)) = self.visual {
            let m = if kind == Visual::Line {
                "VISUAL LINE"
            } else {
                "VISUAL"
            };
            format!("-- {m} --  y yank  o other end  Esc cancel")
        } else {
            "y{motion} yy Y yiw yiq yi{  wbe WBE f/t ; ,  / n N  v V  q close".into()
        };
        format!(" {left}   {right} ")
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::new().cyan())
            .title(Line::from(format!(" {} ", self.title)).bold())
            .title_bottom(self.footer())
            .style(Style::new().bg(POPUP_BG).white());
        let inner = block.inner(area);
        f.render_widget(block, area);

        (self.height, self.width) = (
            (inner.height as usize).max(1),
            (inner.width as usize).max(1),
        );
        self.top = self
            .top
            .clamp(self.row.saturating_sub(self.height - 1), self.row);
        self.left = self
            .left
            .clamp(self.col.saturating_sub(self.width - 1), self.col);

        let (cursor, sel, hit) = (
            Style::new().black().on_cyan(),
            Style::new().on_blue(),
            Style::new().black().on_yellow(),
        );
        let lines: Vec<Line> = (self.top..(self.top + self.height).min(self.lines.len()))
            .map(|r| {
                let l = &self.lines[r];
                let hits = self.hits(r);
                let end = l
                    .len()
                    .max(1)
                    .max(if r == self.row { self.col + 1 } else { 0 });
                (self.left..end.min(self.left + self.width))
                    .map(|c| {
                        let style = if (r, c) == (self.row, self.col) {
                            cursor
                        } else if self.is_selected(r, c) {
                            sel
                        } else if hits.iter().any(|&(s, e)| (s..e).contains(&c)) {
                            hit
                        } else {
                            Style::new()
                        };
                        // one cell per char keeps cols aligned; tabs and control chars stay raw for yanks
                        let ch = match l.get(c) {
                            Some('\t') => ' ',
                            Some(&ch) if ch.is_control() => '?',
                            Some(&ch) => ch,
                            None => ' ',
                        };
                        Span::styled(ch.to_string(), style)
                    })
                    .collect::<Line>()
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn yanked(text: String) -> Action {
    let what = match text.lines().count() {
        n if n > 1 => format!("{n} lines"),
        _ => format!("{} chars", text.chars().count()),
    };
    Action::Yank(text, what)
}

impl Viewer {
    /// Charwise yank; in JSON mode text that is valid string content is unescaped (`a \"b\"` → `a "b"`).
    fn yank_chars(&self, text: String) -> Action {
        if !self.json {
            return yanked(text);
        }
        yanked(serde_json::from_str::<String>(&format!("\"{text}\"")).unwrap_or(text))
    }
}

fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Pretty-print JSON objects/arrays; None for anything else.
// ponytail: duplicate keys collapse to the last one; show raw if that ever matters
fn pretty(s: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
}

/// fzf extended-search syntax: space-separated terms are ANDed and matched in any order.
fn filter(secrets: &[SecretListEntry], query: &str) -> Vec<usize> {
    let m = SkimMatcherV2::default();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let mut scored: Vec<(i64, usize)> = secrets
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let name = s.name().unwrap_or_default();
            terms
                .iter()
                .try_fold(0, |acc, t| Some(acc + term_score(&m, name, t)?))
                .map(|sc| (sc, i))
        })
        .collect();
    scored.sort_by_key(|a| std::cmp::Reverse(a.0)); // stable: ties keep API order
    scored.into_iter().map(|(_, i)| i).collect()
}

/// One term: `foo` fuzzy, `'foo` exact, `^foo` prefix, `foo$` suffix, `!foo` excludes. Smart-case.
fn term_score(m: &SkimMatcherV2, name: &str, term: &str) -> Option<i64> {
    let name = if term.chars().any(char::is_uppercase) {
        name.to_string()
    } else {
        name.to_lowercase()
    };
    let (neg, t) = term.strip_prefix('!').map_or((false, term), |t| (true, t));
    let (exact, t) = t.strip_prefix('\'').map_or((neg, t), |t| (true, t)); // like fzf, `!foo` is exact
    let (pre, t) = t.strip_prefix('^').map_or((false, t), |t| (true, t));
    let (suf, t) = t.strip_suffix('$').map_or((false, t), |t| (true, t));
    if t.is_empty() {
        return Some(0); // half-typed operator like a lone `!`
    }
    let hit = if pre || suf {
        (!pre || name.starts_with(t)) && (!suf || name.ends_with(t))
    } else if exact {
        name.contains(t)
    } else {
        return m.fuzzy_match(&name, t);
    };
    (hit != neg).then_some(0)
}

fn describe(s: &SecretListEntry) -> String {
    let date = |d: Option<&DateTime>| {
        d.and_then(|d| d.fmt(DateTimeFormat::DateTime).ok())
            .unwrap_or("-".into())
    };
    let opt = |o: Option<&str>| o.unwrap_or("-").to_string();
    let mut out = format!(
        "Name:           {}\nARN:            {}\nDescription:    {}\nKMS key:        {}\nCreated:        {}\nLast changed:   {}\nLast accessed:  {}\nRotation:       {}\nOwning service: {}\nPrimary region: {}\n\nTags:\n",
        opt(s.name()),
        opt(s.arn()),
        opt(s.description()),
        opt(s.kms_key_id()),
        date(s.created_date()),
        date(s.last_changed_date()),
        date(s.last_accessed_date()),
        if s.rotation_enabled().unwrap_or(false) {
            "enabled"
        } else {
            "disabled"
        },
        opt(s.owning_service()),
        opt(s.primary_region()),
    );
    if s.tags().is_empty() {
        out.push_str("  (none)\n");
    }
    for t in s.tags() {
        out.push_str(&format!("  {} = {}\n", opt(t.key()), opt(t.value())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed keys; returns the text of the last yank.
    fn keys(v: &mut Viewer, keys: &str) -> Option<String> {
        let mut out = None;
        for c in keys.chars() {
            let code = match c {
                '⏎' => KeyCode::Enter,
                '⎋' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            if let Some(Action::Yank(t, _)) = v.on_key(KeyEvent::new(code, KeyModifiers::NONE)) {
                out = Some(t);
            }
        }
        out
    }

    fn json() -> Viewer {
        let body = pretty(
            r#"{"user":"admin","password":"a \"b\" c","db":{"host":"x.local","port":5432}}"#,
        )
        .unwrap();
        Viewer {
            json: true,
            ..Viewer::new("t".into(), &body)
        }
    }

    #[test]
    fn fuzzy_filter_ranks_and_drops() {
        let mk = |n: &str| SecretListEntry::builder().name(n).build();
        let s = vec![
            mk("prod/db/password"),
            mk("staging/api-key"),
            mk("prod/api-key"),
        ];
        assert_eq!(filter(&s, ""), vec![0, 1, 2]);
        assert_eq!(filter(&s, "prdapi"), vec![2]);
        assert!(filter(&s, "zzz").is_empty());
    }

    #[test]
    fn extended_search_syntax() {
        let mk = |n: &str| SecretListEntry::builder().name(n).build();
        let s = vec![
            mk("gitlab/voize-gmbh/k8s-cluster-internal/access-tokens"),
            mk("gitlab/voize-gmbh/k8s-cluster-staging/access-tokens"),
            mk("prod/Access"),
        ];
        assert_eq!(filter(&s, "access k8s").len(), 2); // terms in any order
        assert_eq!(filter(&s, "access k8s !internal"), vec![1]);
        assert_eq!(filter(&s, "^prod"), vec![2]);
        assert_eq!(filter(&s, "tokens$ 'staging"), vec![1]);
        assert_eq!(filter(&s, "Access"), vec![2]); // smart-case
        assert_eq!(filter(&s, "k8s !").len(), 2); // lone operator is ignored while typing
    }

    #[test]
    fn quote_objects() {
        let mut v = json();
        assert_eq!(keys(&mut v, "jyiq").as_deref(), Some("user")); // cursor before both pairs: next pair
        assert_eq!(keys(&mut v, "$yiq").as_deref(), Some("admin")); // after both pairs: previous pair
        assert_eq!(keys(&mut v, "j$yi\"").as_deref(), Some(r#"a "b" c"#)); // escaped quotes skipped, then unescaped
        assert_eq!(keys(&mut v, "ya\"").as_deref(), Some(r#""a \"b\" c""#));
    }

    #[test]
    fn yanks_match_the_secret() {
        let body = pretty(r#"{"n":123456789012345678901234,"p":"x\\y\nz"}"#).unwrap();
        let mut v = Viewer {
            json: true,
            ..Viewer::new("t".into(), &body)
        };
        assert_eq!(
            keys(&mut v, "j$byiw").as_deref(),
            Some("123456789012345678901234")
        );
        assert_eq!(keys(&mut v, "j$yiq").as_deref(), Some("x\\y\nz"));
        let mut v = Viewer::new("t".into(), "a\tb\r\nc\n");
        assert_eq!(keys(&mut v, "yy").as_deref(), Some("a\tb\r"));
        assert_eq!(v.lines.len(), 2);
        keys(&mut v, "99999999999999999999999j");
        assert_eq!(v.row, 1);
        let mut v = Viewer::new("t".into(), "x x");
        v.last_search = vec!['x'];
        keys(&mut v, "99999n"); // used to spin for ~forever
        assert_eq!(v.col, 2); // odd count over 2 hits, like vim
    }

    #[test]
    fn word_motions_and_objects() {
        let mut v = Viewer::new("t".into(), "foo.bar baz\n  qux-1 end");
        assert_eq!(keys(&mut v, "yw").as_deref(), Some("foo"));
        assert_eq!(keys(&mut v, "yW").as_deref(), Some("foo.bar "));
        assert_eq!(keys(&mut v, "yE").as_deref(), Some("foo.bar"));
        assert_eq!(keys(&mut v, "2yw").as_deref(), Some("foo."));
        assert_eq!(keys(&mut v, "WWyiW").as_deref(), Some("qux-1")); // W crosses the line break
        assert_eq!(keys(&mut v, "yaw").as_deref(), Some("  qux")); // no trailing space: aw takes the leading whitespace
        keys(&mut v, "B");
        assert_eq!((v.row, v.col), (0, 8));
        assert_eq!(keys(&mut v, "yw").as_deref(), Some("baz")); // last word: stops at line end
        assert_eq!(keys(&mut v, "0f.y$").as_deref(), Some(".bar baz"));
        assert_eq!(keys(&mut v, "0ytz").as_deref(), Some("foo.bar ba"));
        assert_eq!(keys(&mut v, "0Y").as_deref(), Some("foo.bar baz"));
    }

    #[test]
    fn brackets_lines_visual_search() {
        let mut v = json();
        assert_eq!(
            keys(&mut v, "4jyi{").as_deref(),
            Some("    \"host\": \"x.local\",\n    \"port\": 5432")
        );
        assert_eq!(
            keys(&mut v, "gg2yy").as_deref(),
            Some("{\n  \"user\": \"admin\",")
        );
        assert_eq!(keys(&mut v, "/x.loc⏎vey").as_deref(), Some("x."));
        assert_eq!(keys(&mut v, "viqy").as_deref(), Some("x.local"));
        assert_eq!(
            keys(&mut v, "Vky").as_deref(),
            Some("  \"db\": {\n    \"host\": \"x.local\",")
        );
        assert_eq!(keys(&mut v, "3G$yiq").as_deref(), Some(r#"a "b" c"#));
        assert_eq!(
            keys(&mut v, "ggjvjoy").as_deref(),
            Some("  \"user\": \"admin\",\n ")
        );
    }
}
