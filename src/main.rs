use indexmap::IndexMap;
use once_cell::sync::Lazy;
use std::{
    env,
    io::{self, BufWriter, Read, Write},
    process,
    sync::Arc,
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Semaphore,
    },
    task,
    time::{self, Duration},
};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use termion::{event::Key, raw::IntoRawMode};

use heif::{HeifContext, LibHeif};
use libheif_rs as heif;

const MAX_FILE_HANDLES: usize = 10;

static HEIF: Lazy<LibHeif> = Lazy::new(LibHeif::new);

#[derive(Clone, Debug)]
enum Event {
    Progress { id: usize, file: Utf8PathBuf },
    Err { id: usize, err: String },
    Quit,
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    last_file: Option<Utf8PathBuf>,
    last_err: Option<String>,

    total: usize,
    completed: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().map(Utf8PathBuf::from).skip(1);
    let input = args
        .next()
        .with_context(|| "missing input directory argument")?;
    let output = args
        .next()
        .with_context(|| "missing output directory argument")?;

    if !output.exists() {
        std::fs::create_dir(&output)?;
    } else if output.read_dir()?.next().is_some() {
        println!("warning: '{}' is not empty", output);
        if !confirm("continue?") {
            process::exit(1);
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<Event>();

    let entries = spawn_file_processors(tx.clone(), &input, &output)?;

    task::spawn(async move {
        let mut stdin = termion::async_stdin().bytes();
        loop {
            if let Some(byte) = stdin.next() {
                let event = termion::event::parse_event(byte.unwrap(), &mut stdin).unwrap();
                if matches!(event, termion::event::Event::Key(Key::Ctrl('c'))) {
                    tx.send(Event::Quit).unwrap();
                    break;
                }
            }
            time::sleep(Duration::from_millis(50)).await;
        }
    });

    let mut stdout = io::stdout().into_raw_mode()?;
    write!(&mut stdout, "{}", termion::cursor::Hide)?;

    let status = event_loop(rx, entries, &mut stdout).await?;

    write!(&mut stdout, "{}", termion::cursor::Show)?;
    process::exit(status)
}

async fn event_loop<W: Write>(
    mut rx: UnboundedReceiver<Event>,
    mut entries: IndexMap<usize, Entry>,
    mut stdout: W,
) -> Result<i32> {
    let mut progress = entries.values().filter(|entry| entry.total > 0).count();

    write!(&mut stdout, "{}", vec!["\n\r"; progress].join(""))?;

    loop {
        let event = rx.recv().await.with_context(|| "event receiver closed")?;

        let mut quit = false;

        match event.clone() {
            Event::Progress { id, file } => {
                let entry = entries.get_mut(&id).unwrap();
                entry.last_file = Some(file);
                entry.last_err = None;

                entry.completed += 1;
                if entry.completed == entry.total {
                    progress -= 1;
                    entry.last_file = None;
                }
            }
            Event::Quit => {
                quit = true;
            }
            Event::Err { id, err } => {
                let entry = entries.get_mut(&id).unwrap();
                entry.last_err = Some(err);
                quit = true;
            }
        }

        render_update(&mut stdout, &entries.values().collect::<Vec<_>>())?;

        if quit {
            break Ok(1);
        } else if progress == 0 {
            break Ok(0);
        }
    }
}

fn render_update<W: Write>(stdout: W, entries: &[&Entry]) -> Result<()> {
    let buf = &mut BufWriter::new(stdout);
    write!(buf, "{}", termion::cursor::Up(entries.len() as u16))?;

    for entry in entries {
        let color = if entry.last_err.is_some() {
            termion::color::Red.fg_str()
        } else if entry.completed == entry.total {
            termion::color::LightGreen.fg_str()
        } else {
            termion::color::LightBlue.fg_str()
        };

        let last_file = entry
            .last_file
            .as_ref()
            .map(|file| format!(" [{}]", file))
            .unwrap_or_default();
        let last_err = entry.last_err.as_deref().unwrap_or_default();

        write!(
            buf,
            "{}{}{} | {:04}/{:04} {} {}\r\n",
            termion::clear::CurrentLine,
            color,
            entry.name,
            entry.completed,
            entry.total,
            last_file,
            last_err
        )?;
    }

    Ok(())
}

fn spawn_file_processors(
    tx: UnboundedSender<Event>,
    input: &Utf8Path,
    output: &Utf8Path,
) -> Result<IndexMap<usize, Entry>> {
    let mut entries = IndexMap::new();

    for (id, dir) in input.read_dir_utf8()?.enumerate() {
        let dir_path = dir?.into_path();
        let dir_name = dir_path.file_name().unwrap().to_string();
        if dir_name == ".MISC" {
            continue;
        }

        let output = output.join(&dir_name);
        if !output.exists() {
            std::fs::create_dir(&output)?;
        }

        let semaphore = Arc::new(Semaphore::new(MAX_FILE_HANDLES));

        let mut total = 0;
        for file in dir_path.read_dir_utf8()? {
            total += 1;

            let source = file?.into_path();
            let file_name = source.file_name().unwrap();
            let ext = source.extension();

            let dest = if ext == Some("HEIC") {
                output.join(file_name).with_extension("PNG")
            } else {
                output.join(file_name)
            };

            let semaphore = semaphore.clone();
            let tx = tx.clone();
            task::spawn(async move {
                let permit = semaphore.acquire().await.unwrap();
                match process_file(&source, &dest).await {
                    Ok(()) => {
                        tx.send(Event::Progress {
                            id,
                            file: source.clone(),
                        })
                        .unwrap();
                    }
                    Err(err) => {
                        tx.send(Event::Err {
                            id,
                            err: format!("{:#}", err),
                        })
                        .unwrap();
                    }
                }
                drop(permit);
            });
        }

        entries.insert(
            id,
            Entry {
                name: dir_name,
                last_file: None,
                last_err: None,

                total,
                completed: 0,
            },
        );
    }

    Ok(entries)
}

async fn process_file(source: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    if source.extension() != Some("HEIC") {
        tokio::fs::copy(source, dest).await?;
    } else {
        let source = source.to_owned();
        let dest = dest.to_owned();

        task::spawn_blocking(move || {
            let file = std::fs::File::create(&dest)?;

            heif_to_png(&source, file)?;

            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }

    Ok(())
}

fn heif_to_png<W: Write>(source: &Utf8Path, writer: W) -> Result<()> {
    let ctx = HeifContext::read_from_file(source.as_str())?;
    let handle = ctx.primary_image_handle()?;

    let image = HEIF.decode(&handle, heif::ColorSpace::Rgb(heif::RgbChroma::Rgb), None)?;
    let planes = image.planes();
    let plane = planes.interleaved.unwrap();

    let target_size = plane.width * plane.height * 3;
    let actual_size = plane.data.len();

    let mut encoder = png::Encoder::new(writer, plane.width, plane.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_compression(png::Compression::Best);

    let mut writer = encoder.write_header()?;
    if target_size as usize == actual_size {
        log(format!("converting {} (full stream)", source));
        let mut stream = writer.stream_writer()?;
        stream.write_all(plane.data)?;
    } else {
        log(format!("converting {} (chunked)", source));
        let chunk_size = actual_size / plane.height as usize;
        let mut stream = writer.stream_writer_with_size(chunk_size)?;
        for chunk in plane.data.chunks_exact(chunk_size) {
            stream.write_all(chunk)?;
        }
    }

    log(format!("done converting {}", source));
    Ok(())
}

fn confirm(msg: &str) -> bool {
    print!("{} [y/N]: ", msg);
    io::stdout().flush().unwrap();

    let mut res = String::new();
    if io::stdin().read_line(&mut res).is_err() {
        return false;
    }
    res.trim() == "y"
}

fn log(msg: impl std::fmt::Display) {
    let mut file = std::fs::File::options()
        .append(true)
        .create(true)
        .open("log.txt")
        .unwrap();
    writeln!(file, "{}", msg).unwrap();
}
