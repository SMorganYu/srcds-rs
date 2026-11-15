//! Garry's Mod Lua dialect → standard Lua 5.1/LuaJIT source.
//!
//! GMod's Lua accepts C-style operators and comments: `!=`, `!`, `&&`, `||`, `//` and
//! `/* */` comments, and a `continue` statement. This module rewrites them token by
//! token (strings, long strings and comments are left untouched) while keeping every
//! line on its original line number, so error messages point at the right place.
//!
//! `continue` becomes `goto __cont_N` with a `::__cont_N::` label placed right before
//! the `end` (or `until`) closing the innermost loop.

/// Rewrites GMod-dialect source to standard Lua.
pub fn to_standard_lua(src: &str) -> String {
    // pass 1 finds the loops that use `continue`; pass 2 wraps their bodies as
    // `do <body> end ::__cont_N::` so the label is always legal (after `break`,
    // after locals) and emits the gotos.
    let used = rewrite(src, None).1;
    rewrite(src, Some(&used)).0
}

fn rewrite(src: &str, wrap: Option<&std::collections::HashSet<u32>>) -> (String, std::collections::HashSet<u32>) {
    let mut used_loops = std::collections::HashSet::new();
    let wraps = |id: u32| wrap.is_some_and(|w| w.contains(&id));
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 64);
    let mut i = 0;
    // block stack: (kind, loop id if it is a loop body, needs continue label)
    let mut blocks: Vec<Block> = Vec::new();
    let mut next_loop_id = 0u32;
    // `for` / `while` open their body at the following `do`
    let mut pending_loop = 0usize;
    while i < b.len() {
        let c = b[i];
        // long string / long comment openers
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            if let Some(level) = long_bracket_level(b, i + 2) {
                let end = find_long_close(b, i + 2 + level + 2, level);
                out.push_str(&src[i..end]);
                i = end;
                continue;
            }
            let end = line_end(b, i);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = line_end(b, i);
            out.push_str("--");
            out.push_str(&src[i + 2..end]);
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            // block comment: keep its newlines so line numbers stay put
            let close = src[i + 2..].find("*/").map(|p| i + 2 + p + 2).unwrap_or(b.len());
            for ch in src[i..close].chars() {
                if ch == '\n' {
                    out.push('\n');
                }
            }
            out.push(' ');
            i = close;
            continue;
        }
        if c == b'[' {
            if let Some(level) = long_bracket_level(b, i) {
                let end = find_long_close(b, i + level + 2, level);
                out.push_str(&src[i..end]);
                i = end;
                continue;
            }
        }
        if c == b'"' || c == b'\'' {
            let end = string_end(b, i);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if c == b'!' {
            if b.get(i + 1) == Some(&b'=') {
                out.push_str("~=");
                i += 2;
            } else {
                out.push_str(" not ");
                i += 1;
            }
            continue;
        }
        if c == b'&' && b.get(i + 1) == Some(&b'&') {
            out.push_str(" and ");
            i += 2;
            continue;
        }
        if c == b'|' && b.get(i + 1) == Some(&b'|') {
            out.push_str(" or ");
            i += 2;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let end = number_end(b, i);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut end = i + 1;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
                end += 1;
            }
            let word = &src[i..end];
            // a name directly after `.`/`:` is a field, not a keyword
            let is_field = prev_non_space(b, i).is_some_and(|p| p == b'.' || p == b':')
                && !src[..i].trim_end().ends_with("..");
            if is_field {
                out.push_str(word);
                i = end;
                continue;
            }
            // GMod compiler macro: DEFINE_BASECLASS( name ) → local BaseClass = baseclass.Get( name )
            if word == "DEFINE_BASECLASS" {
                out.push_str("local BaseClass = baseclass.Get");
                i = end;
                continue;
            }
            match word {
                "function" | "if" => blocks.push(Block::Plain),
                "for" | "while" => pending_loop += 1,
                "do" | "repeat" => {
                    if word == "repeat" || pending_loop > 0 {
                        if word == "do" {
                            pending_loop -= 1;
                        }
                        let id = next_loop_id;
                        next_loop_id += 1;
                        blocks.push(Block::Loop { id, used: false });
                        if wraps(id) {
                            out.push_str(word);
                            out.push_str(" do ");
                            i = end;
                            continue;
                        }
                    } else {
                        blocks.push(Block::Plain);
                    }
                }
                "end" | "until" => {
                    if let Some(Block::Loop { id, .. }) = blocks.last() {
                        if wraps(*id) {
                            out.push_str(&format!("end ::__cont_{id}:: "));
                        }
                    }
                    blocks.pop();
                }
                "continue" => {
                    if let Some(Block::Loop { id, used }) = blocks
                        .iter_mut()
                        .rev()
                        .find(|b| matches!(b, Block::Loop { .. }))
                    {
                        *used = true;
                        used_loops.insert(*id);
                        out.push_str(&format!("goto __cont_{id}"));
                        i = end;
                        continue;
                    }
                }
                _ => {}
            }
            out.push_str(word);
            i = end;
            continue;
        }
        // copy any other character (keeps multi-byte UTF-8 intact)
        let ch_len = utf8_len(c);
        out.push_str(&src[i..(i + ch_len).min(b.len())]);
        i += ch_len;
    }
    (out, used_loops)
}

#[derive(Debug)]
enum Block {
    Plain,
    Loop { id: u32, used: bool },
}

fn utf8_len(c: u8) -> usize {
    match c {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn prev_non_space(b: &[u8], i: usize) -> Option<u8> {
    b[..i].iter().rev().copied().find(|c| !c.is_ascii_whitespace())
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)
}

/// `[` `=`* `[` at `i` → number of `=`.
fn long_bracket_level(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'=') {
        j += 1;
    }
    (b.get(j) == Some(&b'[')).then_some(j - i - 1)
}

fn find_long_close(b: &[u8], from: usize, level: usize) -> usize {
    let mut j = from;
    while j < b.len() {
        if b[j] == b']' {
            let mut k = j + 1;
            let mut eq = 0;
            while b.get(k) == Some(&b'=') {
                eq += 1;
                k += 1;
            }
            if eq == level && b.get(k) == Some(&b']') {
                return k + 1;
            }
        }
        j += 1;
    }
    b.len()
}

fn string_end(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == q => return j + 1,
            b'\n' => return j,
            _ => j += 1,
        }
    }
    b.len()
}

fn number_end(b: &[u8], i: usize) -> usize {
    let mut j = i;
    if b[j] == b'0' && matches!(b.get(j + 1), Some(b'x' | b'X')) {
        j += 2;
        while j < b.len() && (b[j].is_ascii_hexdigit() || b[j] == b'.') {
            j += 1;
        }
        return j;
    }
    while j < b.len() {
        let c = b[j];
        if c.is_ascii_digit() || c == b'.' {
            j += 1;
        } else if (c == b'e' || c == b'E') && j + 1 < b.len() {
            j += 1;
            if b[j] == b'+' || b[j] == b'-' {
                j += 1;
            }
        } else {
            break;
        }
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> String {
        let lua = mlua::Lua::new();
        let code = to_standard_lua(src);
        lua.load(&code)
            .eval::<String>()
            .unwrap_or_else(|e| panic!("{e}\n--- code ---\n{code}"))
    }

    #[test]
    fn operators_and_comments() {
        assert_eq!(
            run("local a = 1 // comment\nif ( a != 2 && !false || false ) then return 'ok' end return 'no'"),
            "ok"
        );
        assert_eq!(run("/* multi\nline */ return 'x'"), "x");
        // operators inside strings are untouched
        assert_eq!(run("return \"a != b && !c // d\""), "a != b && !c // d");
        assert_eq!(run("return [[!= && ||]]"), "!= && ||");
    }

    #[test]
    fn define_baseclass_macro() {
        assert_eq!(
            to_standard_lua("DEFINE_BASECLASS( \"base\" )"),
            "local BaseClass = baseclass.Get( \"base\" )"
        );
    }

    #[test]
    fn keeps_line_numbers() {
        let src = "/* a\nb\nc */\nlocal x = 1\nerror('boom')";
        let lua = mlua::Lua::new();
        let err = lua
            .load(&to_standard_lua(src))
            .set_name("t")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains(":5:"), "{err}");
    }

    #[test]
    fn continue_in_loops() {
        assert_eq!(
            run("local s = '' for i = 1, 5 do if i % 2 == 0 then continue end s = s .. i end return s"),
            "135"
        );
        assert_eq!(
            run("local s, i = '', 0 while i < 4 do i = i + 1 if i == 2 then continue end s = s .. i end return s"),
            "134"
        );
        // nested loops: continue applies to the innermost one
        assert_eq!(
            run("local s='' for a=1,2 do for b=1,3 do if b==2 then continue end s=s..a..b end end return s"),
            "11132123"
        );
        // `continue` as a field name stays a field
        assert_eq!(run("local t = {continue = 'f'} return t.continue"), "f");
        // locals declared after continue are fine (label at block end)
        assert_eq!(
            run("local s='' for i=1,3 do if i==1 then continue end local y=i*2 s=s..y end return s"),
            "46"
        );
    }

    /// Compiles every Lua file of a GMod install after rewriting
    /// (`SRCDS_RS_GMOD_DIR=<path to garrysmod>`).
    #[test]
    fn whole_gmod_tree_compiles() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else {
            return;
        };
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else if p.extension().is_some_and(|x| x == "lua") {
                        out.push(p);
                    }
                }
            }
        }
        let mut files = Vec::new();
        for sub in ["lua", "gamemodes"] {
            walk(&std::path::Path::new(&root).join(sub), &mut files);
        }
        let lua = mlua::Lua::new();
        let mut bad = Vec::new();
        for f in &files {
            let src = String::from_utf8_lossy(&std::fs::read(f).unwrap()).to_string();
            let code = to_standard_lua(&src);
            if let Err(e) = lua.load(&code).set_name(f.to_string_lossy()).into_function() {
                bad.push(format!("{}: {e}", f.display()));
            }
        }
        eprintln!("{} files, {} failed", files.len(), bad.len());
        for b in bad.iter().take(15) {
            eprintln!("  {b}");
        }
        assert!(bad.is_empty());
    }

    #[test]
    fn numbers_and_keywords_in_names() {
        assert_eq!(run("local endx, doit = 1, 2 return tostring(endx + doit + 0x10 + 1e1)"), "29");
        assert_eq!(run("local t = {} t.end_ = 'e' function t:do_() return self.end_ end return t:do_()"), "e");
    }
}
