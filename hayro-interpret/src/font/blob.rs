use crate::font::UNITS_PER_EM;
use crate::font::outline::OutlinePath;
use hayro_font::{Matrix, cff, type1};
use kurbo::{Affine, BezPath};
use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::outline::{DrawSettings, Engine, HintingInstance, HintingOptions, Target};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider, OutlineGlyphCollection};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use yoke::{Yoke, Yokeable};

type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;
type OpenTypeFontYoke = Yoke<OTFYoke<'static>, FontData>;
type CffFontYoke = Yoke<CFFYoke<'static>, FontData>;

/// A font blob for type 1 fonts.
#[derive(Clone)]
pub(crate) struct Type1FontBlob(Arc<type1::Table>);

impl Debug for Type1FontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Type1 Font {{ .. }}")
    }
}

impl Type1FontBlob {
    pub(crate) fn new(data: FontData) -> Option<Self> {
        let table = type1::Table::parse(data.as_ref().as_ref())?;

        Some(Self(Arc::new(table)))
    }

    pub(crate) fn table(&self) -> &type1::Table {
        self.0.as_ref()
    }

    pub(crate) fn outline_glyph(&self, name: &str) -> BezPath {
        let mut path = OutlinePath::new();

        self.table().outline(name, &mut path).unwrap_or_default();

        Affine::scale(UNITS_PER_EM as f64) * convert_matrix(self.table().matrix()) * path.take()
    }
}

/// A font blob for CFF-based fonts.
#[derive(Clone)]
pub(crate) struct CffFontBlob(Arc<CffFontYoke>);

impl Debug for CffFontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Type1 Font {{ .. }}")
    }
}

impl CffFontBlob {
    pub(crate) fn new(data: FontData) -> Option<Self> {
        let _ = cff::Table::parse(data.as_ref().as_ref())?;

        let yoke = Yoke::<CFFYoke<'static>, FontData>::attach_to_cart(data.clone(), |data| {
            let table = cff::Table::parse(data.as_ref()).unwrap();
            CFFYoke { table }
        });

        Some(Self(Arc::new(yoke)))
    }

    pub(crate) fn table(&self) -> &cff::Table<'_> {
        &self.0.as_ref().get().table
    }

    pub(crate) fn outline_glyph(&self, glyph: GlyphId) -> BezPath {
        let mut path = OutlinePath::new();

        let Ok(_) = self
            .table()
            .outline(hayro_font::GlyphId(glyph.to_u32() as u16), &mut path)
        else {
            return BezPath::new();
        };

        Affine::scale(UNITS_PER_EM as f64) * convert_matrix(self.table().matrix()) * path.take()
    }
}

/// A font blob for OpenType fonts.
#[derive(Clone)]
pub(crate) struct OpenTypeFontBlob(Arc<OpenTypeFontYoke>);

impl Debug for OpenTypeFontBlob {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenType Font {{ .. }}")
    }
}

impl OpenTypeFontBlob {
    pub(crate) fn new(data: FontData, index: u32) -> Option<Self> {
        // Check first whether the font is valid so we can unwrap in the closure.
        let f = FontRef::from_index(data.as_ref().as_ref(), index).ok()?;
        // Reject fonts with invalid post table version, fixes pdf.js issue 9462. Not sure if there
        // is a better fix, for some reason skrifa accepts the font which is completely invalid.
        let invalid = f.post().is_ok_and(|p| {
            !matches!(
                p.version().to_major_minor(),
                (1, 0) | (2, 0) | (2, 5) | (3, 0)
            )
        });

        if invalid {
            return None;
        }

        let font_ref_yoke =
            Yoke::<OTFYoke<'static>, FontData>::attach_to_cart(data.clone(), |data| {
                let font_ref = FontRef::from_index(data.as_ref(), index).unwrap();

                let hinting_instance = if font_ref.outline_glyphs().require_interpreter() {
                    HintingInstance::new(
                        &font_ref.outline_glyphs(),
                        Size::new(UNITS_PER_EM),
                        LocationRef::default(),
                        HintingOptions {
                            engine: Engine::Interpreter,
                            target: Target::Mono,
                        },
                    )
                    .ok()
                } else {
                    None
                };

                OTFYoke {
                    font_ref: font_ref.clone(),
                    outline_glyphs: font_ref.outline_glyphs(),
                    hinting_instance,
                    glyph_metrics: font_ref
                        .glyph_metrics(Size::new(UNITS_PER_EM), LocationRef::default()),
                }
            });

        Some(Self(Arc::new(font_ref_yoke)))
    }

    pub(crate) fn font_ref(&self) -> &FontRef<'_> {
        &self.0.as_ref().get().font_ref
    }

    pub(crate) fn glyph_metrics(&self) -> &GlyphMetrics<'_> {
        &self.0.as_ref().get().glyph_metrics
    }

    fn outline_glyphs(&self) -> &OutlineGlyphCollection<'_> {
        &self.0.as_ref().get().outline_glyphs
    }

    pub(crate) fn outline_glyph(&self, glyph: GlyphId) -> BezPath {
        let mut path = OutlinePath::new();

        let draw_settings = if let Some(instance) = self.0.get().hinting_instance.as_ref() {
            // Note: We always hint at the font size `UNITS_PER_EM`, which obviously isn't very useful. We don't do this
            // for better text quality (right now), but instead because there are some PDFs with obscure fonts that
            // actually render wrongly if hinting is disabled!
            DrawSettings::hinted(instance, false)
        } else {
            DrawSettings::unhinted(Size::new(UNITS_PER_EM), LocationRef::default())
        };

        let Some(outline) = self.outline_glyphs().get(glyph) else {
            return BezPath::new();
        };

        let _ = outline.draw(draw_settings, &mut path);
        path.take()
    }

    pub(crate) fn num_glyphs(&self) -> u16 {
        self.font_ref().maxp().map(|m| m.num_glyphs()).unwrap_or(0)
    }
}

fn convert_matrix(matrix: Matrix) -> Affine {
    Affine::new([
        matrix.sx as f64,
        matrix.ky as f64,
        matrix.kx as f64,
        matrix.sy as f64,
        matrix.tx as f64,
        matrix.ty as f64,
    ])
}

#[derive(Yokeable, Clone)]
struct OTFYoke<'a> {
    font_ref: FontRef<'a>,
    glyph_metrics: GlyphMetrics<'a>,
    hinting_instance: Option<HintingInstance>,
    outline_glyphs: OutlineGlyphCollection<'a>,
}

#[derive(Yokeable, Clone)]
struct CFFYoke<'a> {
    table: cff::Table<'a>,
}
