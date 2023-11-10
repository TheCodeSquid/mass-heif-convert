use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexMap;
use once_cell::sync::Lazy;

use std::{
    env, fmt,
    fs::{self, File},
    io::{self, BufWriter, Write},
    process,
    sync::mpsc::{self, Sender},
    thread,
};

use termion::{
    color::Fg,
    cursor,
    event::Key,
    input::TermRead,
    raw::{IntoRawMode, RawTerminal},
};

use heif::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use libheif_rs as heif;

static HEIF: Lazy<LibHeif> = Lazy::new(LibHeif::new);

#[allow(unused)]
enum Ping {
    Quit,
    Status(usize, Status),
}

enum Status {
    Init { total: usize },
    Progress { file: Utf8PathBuf },
}

#[derive(Clone)]
struct Entry {
    name: String,
    file: Option<Utf8PathBuf>,
    total: Option<usize>,
    progress: usize,
}

fn main() -> Result<()> {
    let mut args = env::args().map(Utf8PathBuf::from).skip(1);
    let input = args.next().expect("missing input directory argument");
    let output = args.next().expect("missing output directory argument");

    fs::create_dir(&output)
        .with_context(|| format!("failed to create output directory {}", output))?;

    let subdirs = input
        .read_dir_utf8()
        .with_context(|| format!("failed to read input directory {}", input))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.path().file_name() != Some(".MISC"))
        .collect::<Vec<_>>();

    let mut pending = subdirs.len();
    let (tx, rx) = mpsc::channel();

    ctrlc::set_handler({
        let tx = tx.clone();
        move || tx.send(Ping::Quit).unwrap()
    })?;
    thread::spawn({
        let tx = tx.clone();
        move || {
            let stdin = io::stdin();
            for key in stdin.keys() {
                if matches!(key.unwrap(), Key::Ctrl('c')) {
                    tx.send(Ping::Quit).unwrap();
                    break;
                }
            }
        }
    });

    let mut indices = IndexMap::new();

    for (i, subdir) in subdirs.iter().enumerate() {
        let path = subdir.path().to_owned();
        let name = path.file_name().unwrap().to_string();
        let out = Utf8PathBuf::from(&output);

        indices.insert(i, name.clone());

        let tx = tx.clone();

        thread::Builder::new()
            .name(name.clone())
            .spawn(move || process_dir(i, tx, &path, &out.join(name)))?;
    }

    let names: IndexMap<_, _> = indices.sorted_by(|_, v1, _, v2| v1.cmp(v2)).collect();
    log(format!("{:#?}", names));

    let mut stdout = io::stdout().into_raw_mode()?;
    write!(
        &mut stdout,
        "{}{}",
        termion::cursor::Hide,
        vec!["\n"; names.len()].join("")
    )?;

    let mut entries: IndexMap<_, _> = names
        .iter()
        .map(|(k, v)| {
            (
                *k,
                Entry {
                    name: v.clone(),
                    file: None,
                    total: None,
                    progress: 0,
                },
            )
        })
        .collect();

    let mut quit = false;
    while pending != 0 {
        let ping = rx.recv()?;

        match ping {
            Ping::Quit => {
                quit = true;
                break;
            }
            Ping::Status(i, status) => match status {
                Status::Init { total } => {
                    entries.get_mut(&i).unwrap().total = Some(total);
                    if total == 0 {
                        pending -= 1;
                    }
                }
                Status::Progress { file } => {
                    let entry = entries.get_mut(&i).unwrap();
                    entry.progress += 1;
                    entry.file = Some(file);

                    if entry.progress == entry.total.unwrap() {
                        pending -= 1;
                        entry.file = None;
                    }
                }
            },
        }

        render_update(&mut stdout, &entries)?;
    }

    write!(&mut stdout, "{}", termion::cursor::Show)?;
    drop(stdout);
    println!();

    if quit {
        process::exit(1);
    }
    Ok(())
}

fn render_update<W: Write>(
    stdout: &mut RawTerminal<W>,
    entries: &IndexMap<usize, Entry>,
) -> Result<()> {
    let len = entries.len() as u16;

    let mut buf = BufWriter::new(stdout);
    write!(&mut buf, "{}", cursor::Up(len))?;

    for entry in entries.values() {
        let total_str = if let Some(total) = entry.total {
            if total == 0 {
                write!(&mut buf, "{}", Fg(termion::color::LightBlack))?;
            } else if total == entry.progress {
                write!(&mut buf, "{}", Fg(termion::color::LightGreen))?;
            } else {
                write!(&mut buf, "{}", Fg(termion::color::LightBlue))?;
            }

            format!("{:04}", total)
        } else {
            "???".to_string()
        };

        let file = if let Some(file) = &entry.file {
            format!("[{}]", file)
        } else {
            "".to_string()
        };

        write!(
            &mut buf,
            "{}{} - {:04}/{}  {}\r\n{}",
            termion::clear::CurrentLine,
            entry.name,
            entry.progress,
            total_str,
            file,
            termion::color::Reset.fg_str(),
        )?;
    }

    Ok(())
}

fn process_dir(index: usize, tx: Sender<Ping>, path: &Utf8Path, out: &Utf8Path) {
    fs::create_dir(out)
        .with_context(|| format!("failed to create output subdirectory {}", out))
        .unwrap();

    let entries = path
        .read_dir_utf8()
        .with_context(|| format!("failed to read input subdirectory {}", path))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    tx.send(Ping::Status(
        index,
        Status::Init {
            total: entries.len(),
        },
    ))
    .unwrap();

    let entries = entries.into_iter().peekable();

    for entry in entries {
        let path = entry.path().to_owned();
        let name = path.file_name().unwrap().to_owned();
        let ext = path.extension().unwrap().to_owned();

        let out = out.to_owned();
        let tx = tx.clone();

        rayon::spawn(move || {
            if ext != "HEIC" {
                fs::copy(&path, out.join(name)).unwrap();
            } else {
                let path_png = path.with_extension("PNG");
                let name_png = path_png.file_name().unwrap();
                let mut file = File::create(out.join(name_png)).unwrap();
                convert_heif(&path, &mut file)
                    .with_context(|| format!("failed to convert image {}", path))
                    .unwrap();
            }

            tx.send(Ping::Status(index, Status::Progress { file: path }))
                .unwrap();
        });
    }
}

fn convert_heif<W: Write>(path: &Utf8Path, writer: W) -> Result<()> {
    let ctx = HeifContext::read_from_file(path.as_str())?;
    let handle = ctx.primary_image_handle()?;

    let image = HEIF.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)?;

    let planes = image.planes();
    let plane = planes.interleaved.unwrap();

    let target_size = plane.width * plane.height * 3;
    let chunk_size = if target_size as usize != plane.data.len() {
        plane.data.len() / plane.height as usize
    } else {
        target_size as usize
    };

    let offset = if chunk_size == target_size as usize {
        0
    } else {
        3
    };

    let mut encoder = png::Encoder::new(writer, plane.width, plane.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder.write_header()?;

    if offset == 0 {
        writer.stream_writer()?.write_all(plane.data)?;
    } else {
        let mut writer = writer.stream_writer_with_size(chunk_size)?;
        for chunk in plane.data.chunks(chunk_size) {
            writer.write_all(chunk)?;
        }
    }
    Ok(())
}

fn log<S: fmt::Display>(msg: S) {
    let mut file = File::options()
        .create(true)
        .append(true)
        .open("log.txt")
        .unwrap();
    writeln!(file, "{}", msg).unwrap();
}
