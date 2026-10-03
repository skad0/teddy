//! Core layout for plugin widgets (spec §14.3). Plugins send data only; the
//! core decides every cell and writes plain row bytes, so `render::paint`
//! stays the single emitter and plugin text is escaped like file text.

use crate::plugin::{
    Widget, TREE_EXPANDED, TREE_HAS_CHILDREN, W_ACTIONS, W_LOG, W_PROMPT, W_TABLE, W_TEXT, W_TREE,
};

/// How many positions Up/Down walk: list/tree rows, table body rows, text
/// scroll offsets, action buttons. Logs follow the tail; prompts take text.
pub fn selectable(w: &Widget) -> usize {
    match w.kind {
        W_TABLE => (w.items.len() / w.cols.max(1) as usize).saturating_sub(1),
        W_LOG | W_PROMPT => 0,
        _ => w.items.len(),
    }
}

/// First visible index that keeps `sel` on screen within `h` rows.
fn top_for(sel: usize, h: usize) -> usize {
    (sel + 1).saturating_sub(h)
}

/// Lay `w` out into `rows` (one fixed slot, no plugin-controlled geometry).
/// `sel` is the cursor from `selectable`; `input` is the prompt/search text.
pub fn draw(
    w: &Widget,
    sel: usize,
    input: &str,
    rows: &mut [Vec<u8>],
    row_sel: &mut [Option<(usize, usize)>],
) {
    rows.iter_mut().for_each(Vec::clear);
    row_sel.iter_mut().for_each(|s| *s = None);
    let h = rows.len();
    if h == 0 {
        return;
    }
    // the widget may have shrunk since the cursor last moved
    let sel = sel.min(selectable(w).saturating_sub(1));
    let highlight = |rows: &mut [Vec<u8>], row_sel: &mut [Option<(usize, usize)>], r: usize| {
        row_sel[r] = Some((0, rows[r].len()));
    };
    match w.kind {
        W_ACTIONS => {
            let row = &mut rows[0];
            for (i, label) in w.items.iter().enumerate() {
                let start = row.len();
                row.extend_from_slice(b"[ ");
                row.extend_from_slice(label.as_bytes());
                row.extend_from_slice(b" ]");
                if i == sel {
                    row_sel[0] = Some((start, row.len()));
                }
                row.extend_from_slice(b"  ");
            }
        }
        W_PROMPT => {
            let label = w.items.first().map_or("", String::as_str);
            rows[0].extend_from_slice(format!("{label}: {input}").as_bytes());
        }
        W_LOG => {
            let start = w.items.len().saturating_sub(h);
            for (row, line) in rows.iter_mut().zip(&w.items[start..]) {
                row.extend_from_slice(line.as_bytes());
            }
        }
        W_TEXT => {
            let top = sel.min(w.items.len().saturating_sub(h));
            for (row, line) in rows.iter_mut().zip(w.items.iter().skip(top)) {
                row.extend_from_slice(line.as_bytes());
            }
        }
        W_TABLE => {
            let cols = w.cols.max(1) as usize;
            // ponytail: char count as width, like the renderer (no wide-char table).
            let mut widths = vec![0usize; cols];
            for (i, cell) in w.items.iter().enumerate() {
                widths[i % cols] = widths[i % cols].max(cell.chars().count());
            }
            let table_row = |row: &mut Vec<u8>, cells: &[String]| {
                for (c, cell) in cells.iter().enumerate() {
                    row.extend_from_slice(cell.as_bytes());
                    if c + 1 < cells.len() {
                        let pad = widths[c] - cell.chars().count() + 2;
                        row.extend(std::iter::repeat_n(b' ', pad));
                    }
                }
            };
            let mut chunks = w.items.chunks(cols);
            if let Some(header) = chunks.next() {
                table_row(&mut rows[0], header);
            }
            let top = top_for(sel, h - 1);
            for (r, cells) in chunks.enumerate().skip(top).take(h - 1) {
                let at = r - top + 1;
                table_row(&mut rows[at], cells);
                if r == sel {
                    highlight(rows, row_sel, at);
                }
            }
        }
        _ => {
            // list and tree
            let top = top_for(sel, h);
            for (i, item) in w.items.iter().enumerate().skip(top).take(h) {
                let at = i - top;
                if w.kind == W_TREE {
                    let (depth, flags) = w.tree.get(i).copied().unwrap_or((0, 0));
                    rows[at].extend(std::iter::repeat_n(b' ', 2 * depth as usize));
                    let marker: &[u8] = match (flags & TREE_HAS_CHILDREN, flags & TREE_EXPANDED) {
                        (0, _) => b"  ",
                        (_, 0) => "▸ ".as_bytes(),
                        _ => "▾ ".as_bytes(),
                    };
                    rows[at].extend_from_slice(marker);
                }
                rows[at].extend_from_slice(item.as_bytes());
                if i == sel {
                    highlight(rows, row_sel, at);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{encode_widget, parse_widget, W_LIST};

    fn w(kind: u8, cols: u8, items: &[&str], tree: &[(u8, u8)]) -> Widget {
        Widget {
            kind,
            cols,
            items: items.iter().map(|s| s.to_string()).collect(),
            tree: tree.to_vec(),
            ..Widget::default()
        }
    }

    fn render(
        w: &Widget,
        sel: usize,
        input: &str,
        h: usize,
    ) -> (Vec<String>, Vec<Option<(usize, usize)>>) {
        let mut rows = vec![b"stale".to_vec(); h];
        let mut row_sel = vec![Some((0, 1)); h];
        draw(w, sel, input, &mut rows, &mut row_sel);
        let text = rows
            .into_iter()
            .map(|r| String::from_utf8(r).unwrap())
            .collect();
        (text, row_sel)
    }

    #[test]
    fn every_kind_round_trips_through_the_codec() {
        let all = [
            w(W_LIST, 0, &["a", "b"], &[]),
            w(W_TREE, 0, &["src", "main.rs"], &[(0, 3), (1, 0)]),
            w(W_TABLE, 2, &["name", "size", "a", "1"], &[]),
            w(W_TEXT, 0, &["para"], &[]),
            w(W_LOG, 0, &["l1", "l2"], &[]),
            w(W_PROMPT, 0, &["Name"], &[]),
            w(W_ACTIONS, 0, &["OK", "Cancel"], &[]),
        ];
        for widget in &all {
            assert_eq!(parse_widget(&encode_widget(widget)).as_ref(), Some(widget));
        }
    }

    #[test]
    fn malformed_shapes_are_rejected() {
        // unknown kind
        assert!(parse_widget(&[0, 0, 0]).is_none());
        assert!(parse_widget(&[8, 0, 0]).is_none());
        // table: zero cols, ragged rows, missing header
        assert!(parse_widget(&encode_widget(&w(W_TABLE, 0, &[], &[]))).is_none());
        assert!(parse_widget(&encode_widget(&w(W_TABLE, 2, &["a", "b", "c"], &[]))).is_none());
        assert!(parse_widget(&encode_widget(&w(W_TABLE, 2, &[], &[]))).is_none());
        // prompt needs exactly one label
        assert!(parse_widget(&encode_widget(&w(W_PROMPT, 0, &[], &[]))).is_none());
        // tree: truncated meta, absurd depth
        let tree = encode_widget(&w(W_TREE, 0, &["x"], &[(0, 0)]));
        assert!(parse_widget(&tree[..4]).is_none());
        assert!(parse_widget(&encode_widget(&w(W_TREE, 0, &["x"], &[(33, 0)]))).is_none());
        // tree: unknown flag bits, expanded without children
        assert!(parse_widget(&encode_widget(&w(W_TREE, 0, &["x"], &[(0, 4)]))).is_none());
        assert!(parse_widget(&encode_widget(&w(W_TREE, 0, &["x"], &[(0, 2)]))).is_none());
        // trailing bytes
        let mut list = encode_widget(&w(W_LIST, 0, &["a"], &[]));
        list.push(0);
        assert!(parse_widget(&list).is_none());
    }

    #[test]
    fn strings_are_sanitized_in_every_kind() {
        let table = encode_widget(&w(W_TABLE, 1, &["h\x1b[2J"], &[]));
        assert_eq!(parse_widget(&table).unwrap().items, vec!["h?[2J"]);
        let prompt = encode_widget(&w(W_PROMPT, 0, &["a\tb"], &[]));
        assert_eq!(parse_widget(&prompt).unwrap().items, vec!["a?b"]);
    }

    #[test]
    fn tree_indents_marks_and_scrolls_to_selection() {
        let t = w(
            W_TREE,
            0,
            &["src", "main.rs", "docs", "x"],
            &[(0, 3), (1, 0), (0, 1), (0, 0)],
        );
        let (rows, sel) = render(&t, 0, "", 4);
        assert_eq!(rows, vec!["▾ src", "    main.rs", "▸ docs", "  x"]);
        assert_eq!(sel[0], Some((0, "▾ src".len())));
        assert!(sel[1..].iter().all(Option::is_none));
        let (rows, sel) = render(&t, 3, "", 2);
        assert_eq!(rows, vec!["▸ docs", "  x"]);
        assert_eq!(sel, vec![None, Some((0, 3))]);
        // a stale cursor past a shrunken widget still lands on its last row
        let (rows, sel) = render(&t, 99, "", 2);
        assert_eq!(rows, vec!["▸ docs", "  x"]);
        assert_eq!(sel, vec![None, Some((0, 3))]);
    }

    #[test]
    fn table_aligns_columns_and_keeps_header() {
        let t = w(
            W_TABLE,
            2,
            &["name", "size", "a", "1", "longer", "22", "c", "3"],
            &[],
        );
        assert_eq!(selectable(&t), 3);
        let (rows, sel) = render(&t, 2, "", 3);
        assert_eq!(rows, vec!["name    size", "longer  22", "c       3"]);
        assert_eq!(sel, vec![None, None, Some((0, 9))]);
    }

    #[test]
    fn log_follows_tail_and_text_scrolls() {
        let log = w(W_LOG, 0, &["1", "2", "3"], &[]);
        assert_eq!(selectable(&log), 0);
        assert_eq!(render(&log, 0, "", 2).0, vec!["2", "3"]);
        assert_eq!(render(&log, 0, "", 5).0, vec!["1", "2", "3", "", ""]);
        let text = w(W_TEXT, 0, &["a", "b", "c"], &[]);
        assert_eq!(render(&text, 1, "", 2).0, vec!["b", "c"]);
        assert_eq!(render(&text, 9, "", 2).0, vec!["b", "c"]);
        assert!(render(&text, 0, "", 2).1.iter().all(Option::is_none));
    }

    #[test]
    fn prompt_and_actions_render_on_one_row() {
        let p = w(W_PROMPT, 0, &["Rename"], &[]);
        assert_eq!(render(&p, 0, "foo", 2).0, vec!["Rename: foo", ""]);
        let a = w(W_ACTIONS, 0, &["OK", "Cancel"], &[]);
        let (rows, sel) = render(&a, 1, "", 1);
        assert_eq!(rows, vec!["[ OK ]  [ Cancel ]  "]);
        assert_eq!(sel, vec![Some((8, 18))]);
    }
}
