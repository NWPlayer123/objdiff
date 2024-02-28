use std::{
    io::{stdout, Write},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use argp::FromArgs;
use crossterm::{
    cursor::{Hide, MoveRight, MoveTo, Show},
    event,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
    },
    style::{Color, PrintStyledContent, Stylize},
    terminal::{
        disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType,
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use event::KeyModifiers;
use objdiff_core::{
    diff,
    diff::display::{display_diff, DiffText},
    obj,
    obj::{ObjInfo, ObjInsDiffKind, ObjSection, ObjSectionKind, ObjSymbol},
};

use crate::util::term::crossterm_panic_handler;

#[derive(FromArgs, PartialEq, Debug)]
/// Diff two object files.
#[argp(subcommand, name = "diff")]
pub struct Args {
    #[argp(positional)]
    /// Target object file
    target: PathBuf,
    #[argp(positional)]
    /// Base object file
    base: PathBuf,
    #[argp(option, short = 's')]
    /// Function symbol to diff
    symbol: String,
}

pub fn run(args: Args) -> Result<()> {
    let mut target = obj::elf::read(&args.target)
        .with_context(|| format!("Loading {}", args.target.display()))?;
    let mut base =
        obj::elf::read(&args.base).with_context(|| format!("Loading {}", args.base.display()))?;
    let config = diff::DiffObjConfig::default();
    diff::diff_objs(&config, Some(&mut target), Some(&mut base))?;

    let left_sym = find_function(&target, &args.symbol);
    let right_sym = find_function(&base, &args.symbol);
    let max_len = match (left_sym, right_sym) {
        (Some((_, l)), Some((_, r))) => l.instructions.len().max(r.instructions.len()),
        (Some((_, l)), None) => l.instructions.len(),
        (None, Some((_, r))) => r.instructions.len(),
        (None, None) => bail!("Symbol not found: {}", args.symbol),
    };

    crossterm_panic_handler();
    enable_raw_mode()?;
    crossterm::queue!(
        stdout(),
        EnterAlternateScreen,
        SetTitle(format!("{} - objdiff", args.symbol)),
        Hide,
        EnableMouseCapture,
    )?;

    let mut redraw = true;
    let mut skip = 0;
    loop {
        let y_offset = 2;
        let (sx, sy) = terminal_size()?;
        let per_page = sy as usize - y_offset;
        if redraw {
            let mut w = stdout().lock();
            crossterm::queue!(
                w,
                Clear(ClearType::All),
                MoveTo(0, 0),
                PrintStyledContent(args.symbol.clone().with(Color::White)),
                MoveTo(0, 1),
                PrintStyledContent(" ".repeat(sx as usize).underlined()),
                MoveTo(0, 1),
                PrintStyledContent("TARGET ".underlined()),
                MoveTo(sx / 2, 0),
                PrintStyledContent("Last built: 18:24:20".with(Color::White)),
                MoveTo(sx / 2, 1),
                PrintStyledContent("BASE ".underlined()),
            )?;
            if let Some(percent) = right_sym.and_then(|(_, s)| s.match_percent) {
                crossterm::queue!(
                    w,
                    PrintStyledContent(
                        format!("{:.2}%", percent).with(match_percent_color(percent)).underlined()
                    )
                )?;
            }

            if skip > max_len - per_page {
                skip = max_len - per_page;
            }
            if let Some((_, symbol)) = left_sym {
                print_sym(&mut w, symbol, 0, y_offset as u16, sx / 2 - 1, sy, skip)?;
            }
            if let Some((_, symbol)) = right_sym {
                print_sym(&mut w, symbol, sx / 2, y_offset as u16, sx, sy, skip)?;
            }
            w.flush()?;
            redraw = false;
        }

        match event::read()? {
            Event::Key(event)
                if matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                match event.code {
                    // Quit
                    KeyCode::Esc | KeyCode::Char('q') => break,
                    // Page up
                    KeyCode::PageUp => {
                        skip = skip.saturating_sub(per_page);
                        redraw = true;
                    }
                    // Page up (shift + space)
                    KeyCode::Char(' ') if event.modifiers.contains(KeyModifiers::SHIFT) => {
                        skip = skip.saturating_sub(per_page);
                        redraw = true;
                    }
                    // Page down
                    KeyCode::Char(' ') | KeyCode::PageDown => {
                        skip += per_page;
                        redraw = true;
                    }
                    // Scroll down
                    KeyCode::Down | KeyCode::Char('j') => {
                        skip += 1;
                        redraw = true;
                    }
                    // Scroll up
                    KeyCode::Up | KeyCode::Char('k') => {
                        skip = skip.saturating_sub(1);
                        redraw = true;
                    }
                    // Scroll to start
                    KeyCode::Char('g') => {
                        skip = 0;
                        redraw = true;
                    }
                    // Scroll to end
                    KeyCode::Char('G') => {
                        skip = max_len;
                        redraw = true;
                    }
                    _ => {}
                }
            }
            Event::Mouse(event) => match event.kind {
                MouseEventKind::ScrollDown => {
                    skip += 3;
                    redraw = true;
                }
                MouseEventKind::ScrollUp => {
                    skip = skip.saturating_sub(3);
                    redraw = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => redraw = true,
            _ => {}
        }
    }

    // Reset terminal
    crossterm::execute!(stdout(), LeaveAlternateScreen, Show, DisableMouseCapture)?;
    disable_raw_mode()?;
    Ok(())
}

fn find_function<'a>(obj: &'a ObjInfo, name: &str) -> Option<(&'a ObjSection, &'a ObjSymbol)> {
    for section in &obj.sections {
        if section.kind != ObjSectionKind::Code {
            continue;
        }
        for symbol in &section.symbols {
            if symbol.name == name {
                return Some((section, symbol));
            }
        }
    }
    None
}

fn print_sym<W>(
    w: &mut W,
    symbol: &ObjSymbol,
    sx: u16,
    mut sy: u16,
    max_sx: u16,
    max_sy: u16,
    skip: usize,
) -> Result<()>
where
    W: Write,
{
    let base_addr = symbol.address as u32;
    for ins_diff in symbol.instructions.iter().skip(skip) {
        let mut sx = sx;
        if ins_diff.kind != ObjInsDiffKind::None && sx > 2 {
            crossterm::queue!(w, MoveTo(sx - 2, sy))?;
            let s = match ins_diff.kind {
                ObjInsDiffKind::Delete => "< ",
                ObjInsDiffKind::Insert => "> ",
                _ => "| ",
            };
            crossterm::queue!(w, PrintStyledContent(s.with(Color::DarkGrey)))?;
        } else {
            crossterm::queue!(w, MoveTo(sx, sy))?;
        }
        display_diff(ins_diff, base_addr, |text| {
            let mut label_text;
            let mut base_color = match ins_diff.kind {
                ObjInsDiffKind::None | ObjInsDiffKind::OpMismatch | ObjInsDiffKind::ArgMismatch => {
                    Color::Grey
                }
                ObjInsDiffKind::Replace => Color::DarkCyan,
                ObjInsDiffKind::Delete => Color::DarkRed,
                ObjInsDiffKind::Insert => Color::DarkGreen,
            };
            let mut pad_to = 0;
            match text {
                DiffText::Basic(text) => {
                    label_text = text.to_string();
                }
                DiffText::BasicColor(s, idx) => {
                    label_text = s.to_string();
                    base_color = COLOR_ROTATION[idx % COLOR_ROTATION.len()];
                }
                DiffText::Line(num) => {
                    label_text = format!("{num} ");
                    base_color = Color::DarkGrey;
                    pad_to = 5;
                }
                DiffText::Address(addr) => {
                    label_text = format!("{:x}:", addr);
                    pad_to = 5;
                }
                DiffText::Opcode(mnemonic, _op) => {
                    label_text = mnemonic.to_string();
                    if ins_diff.kind == ObjInsDiffKind::OpMismatch {
                        base_color = Color::Blue;
                    }
                    pad_to = 8;
                }
                DiffText::Argument(arg, diff) => {
                    label_text = arg.to_string();
                    if let Some(diff) = diff {
                        base_color = COLOR_ROTATION[diff.idx % COLOR_ROTATION.len()]
                    }
                }
                DiffText::BranchTarget(addr) => {
                    label_text = format!("{addr:x}");
                }
                DiffText::Symbol(sym) => {
                    let name = sym.demangled_name.as_ref().unwrap_or(&sym.name);
                    label_text = name.clone();
                    base_color = Color::White;
                }
                DiffText::Spacing(n) => {
                    crossterm::queue!(w, MoveRight(n as u16))?;
                    sx += n as u16;
                    return Ok(());
                }
                DiffText::Eol => {
                    sy += 1;
                    return Ok(());
                }
            }
            let len = label_text.len();
            if sx >= max_sx {
                return Ok(());
            }
            label_text.truncate(max_sx as usize - sx as usize);
            crossterm::queue!(w, PrintStyledContent(label_text.with(base_color)))?;
            sx += len as u16;
            if pad_to > len {
                let pad = (pad_to - len) as u16;
                crossterm::queue!(w, MoveRight(pad))?;
                sx += pad;
            }
            Ok(())
        })?;
        if sy >= max_sy {
            break;
        }
    }
    Ok(())
}

pub const COLOR_ROTATION: [Color; 8] = [
    Color::Magenta,
    Color::Cyan,
    Color::Green,
    Color::Red,
    Color::Yellow,
    Color::DarkMagenta,
    Color::Blue,
    Color::Green,
];

pub fn match_percent_color(match_percent: f32) -> Color {
    if match_percent == 100.0 {
        Color::Green
    } else if match_percent >= 50.0 {
        Color::Blue
    } else {
        Color::Red
    }
}