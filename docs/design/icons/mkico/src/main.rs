//! Renders docs/design/icons/*.svg to the three .ico files the service embeds
//! (crates/replaycut/assets) plus PNG previews and a contact sheet.
//!
//!   cd docs/design/icons/mkico && cargo run --release -- [out-dir]
//!
//! Sizes 16 and 20 use the `-small` variant of each SVG when it exists.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, Transform};
use resvg::usvg::{Options, Tree};

const SIZES: [u32; 6] = [16, 20, 24, 32, 48, 256];
const STATES: [(&str, &str); 3] = [
    ("icon", "replaycut.ico"),
    ("icon-busy", "tray-busy.ico"),
    ("icon-error", "tray-error.ico"),
];

fn main() -> Result<(), Box<dyn Error>> {
    let icons = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // With an explicit output directory only the .ico files are written;
    // the default `icons/out` also gets the PNG previews and the sheet.
    let explicit = std::env::args().nth(1).map(PathBuf::from);
    let previews = explicit.is_none();
    let out = explicit.unwrap_or_else(|| icons.join("out"));
    fs::create_dir_all(&out)?;

    let mut sheet = Sheet::new();
    for (state, ico_name) in STATES {
        let big = load(&icons.join(format!("{state}.svg")))?;
        let small = load(&icons.join(format!("{state}-small.svg"))).ok();
        let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
        for size in SIZES {
            let tree = match (&small, size <= 20) {
                (Some(s), true) => s,
                _ => &big,
            };
            let pixmap = render(tree, size);
            let png = pixmap.encode_png()?;
            if previews {
                fs::write(out.join(format!("{state}-{size}.png")), &png)?;
            }
            let image = ico::IconImage::read_png(&png[..])?;
            // BMP entries for the small sizes (every icon reader understands them),
            // PNG for 256 px to keep the file small.
            let entry = if size >= 256 {
                ico::IconDirEntry::encode_as_png(&image)?
            } else {
                ico::IconDirEntry::encode_as_bmp(&image)?
            };
            dir.add_entry(entry);
            if size <= 32 {
                sheet.add(&pixmap);
            }
        }
        let path = out.join(ico_name);
        dir.write(fs::File::create(&path)?)?;
        println!("{} ({} entries)", path.display(), SIZES.len());
    }
    if previews {
        fs::write(out.join("sheet.png"), sheet.finish().encode_png()?)?;
        println!("{}", out.join("sheet.png").display());
    }
    Ok(())
}

fn load(path: &Path) -> Result<Tree, Box<dyn Error>> {
    let data = fs::read(path)?;
    Ok(Tree::from_data(&data, &Options::default())?)
}

fn render(tree: &Tree, size: u32) -> Pixmap {
    let mut pixmap = Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// Contact sheet: every small size once on a dark and once on a light task bar.
struct Sheet {
    dark: Pixmap,
    light: Pixmap,
    x: u32,
}

impl Sheet {
    const H: u32 = 56;
    const W: u32 = 3 * (16 + 20 + 24 + 32 + 4 * 12) + 24;

    fn new() -> Self {
        let mut dark = Pixmap::new(Self::W, Self::H).unwrap();
        dark.fill(Color::from_rgba8(32, 32, 32, 255));
        let mut light = Pixmap::new(Self::W, Self::H).unwrap();
        light.fill(Color::from_rgba8(238, 238, 238, 255));
        Self { dark, light, x: 12 }
    }

    fn add(&mut self, icon: &Pixmap) {
        let y = ((Self::H - icon.height()) / 2) as i32;
        for target in [&mut self.dark, &mut self.light] {
            target.draw_pixmap(
                self.x as i32,
                y,
                icon.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        self.x += icon.width() + 12;
    }

    fn finish(self) -> Pixmap {
        let mut all = Pixmap::new(Self::W, Self::H * 2).unwrap();
        all.draw_pixmap(
            0,
            0,
            self.dark.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        all.draw_pixmap(
            0,
            Self::H as i32,
            self.light.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        all
    }
}
