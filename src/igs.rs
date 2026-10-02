//! Blu-ray interactive graphics: the buttons and pictures an HDMV menu shows
//! over its background video.
//!
//! An interactive graphics stream is a sequence of segments, each led by its
//! type (8 bits) and length (16 bits): palettes (PDS), run-length coded
//! objects (ODS), the interactive composition (ICS) and an end segment. A
//! display set is an ICS, the palettes and objects it uses and an end
//! segment; the ICS and large objects are split over several segments, which
//! are joined again. The composition lists pages; each page has button
//! overlap groups (BOGs), of which one button each is shown, drawn with the
//! object of its state: normal, or selected for the button selected when the
//! page appears.
//!
//! `Collector` gathers the first display set of a stream,
//! `Collector::into_menu` reads what its first page shows, and `Menu::draw`
//! blends that onto a decoded frame, the way a player shows the menu once it
//! has settled without user input (libbluray `graphics_controller.c`): a
//! button animation that plays once stays on its last object, one that
//! repeats is shown at its first, and page effects are left out. A pop-up
//! menu, which shows only on request, gives nothing.
//!
//! Layouts follow libbluray (`ig_decode.c`, `pg_decode.c`,
//! `graphics_processor.c`). Every count, length and size is checked against
//! the data, which comes from an untrusted image, and the work is bounded:
//! `MAX_BYTES` of segments and `MAX_OBJECTS` objects are kept, and
//! `MAX_PIXELS_PER_PLANE` planes' worth of runs and pixels drawn.

use std::collections::HashMap;

use crate::mpeg2::{ColorMatrix, Frame};

/// Segment types.
const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const ICS: u8 = 0x18;
const END: u8 = 0x80;

/// Segment data kept for one display set.
pub const MAX_BYTES: usize = 8 << 20;
/// Largest graphics plane (the video size of Blu-ray menus).
const MAX_PLANE_WIDTH: u16 = 1920;
const MAX_PLANE_HEIGHT: u16 = 1080;
/// Pixels drawn, as a multiple of the plane's size: the buttons of real menus
/// cover it about once at most.
const MAX_PIXELS_PER_PLANE: usize = 4;
/// No button or object (an id reference of 0xFFFF).
const NONE: u16 = 0xFFFF;
/// Objects drawn (a page has at most 255 BOGs, one button shown of each).
const MAX_DRAWS: usize = 256;
/// Distinct objects kept for one display set. Real menus use a few hundred
/// at most (every frame of every button animation is one).
const MAX_OBJECTS: usize = 4096;
/// Object ids from this one on name no object (libbluray draws an
/// animation's end object only below it).
const LAST_OBJECT: u16 = 0xFFFE;

/// A sequence descriptor: whether a segment holds the first and the last
/// part of a split ICS or object.
fn first_last(b: u8) -> (bool, bool) {
    (b & 0x80 != 0, b & 0x40 != 0)
}

/// A palette: (Y, Cr, Cb, alpha) per entry, alpha 0 (transparent) where
/// not defined.
type Palette = [[u8; 4]; 256];

struct Object {
    /// object_data: its length (24 bits), width, height and the run-length
    /// coded lines, joined from all parts.
    data: Vec<u8>,
    complete: bool,
}

/// Gathers the segments of the first display set of a graphics stream.
#[derive(Default)]
pub struct Collector {
    /// ICS fields before its data: video_descriptor and
    /// composition_descriptor.
    head: Option<[u8; 8]>,
    /// interactive_composition, joined from all parts.
    composition: Vec<u8>,
    composition_complete: bool,
    palettes: Vec<(u8, Palette)>,
    objects: Vec<Object>,
    /// Index in `objects` of each object id.
    index: HashMap<u16, usize>,
    /// The end segment of the display set came.
    done: bool,
    kept: usize,
}

impl Collector {
    /// Whether the display set is complete (or no more is taken).
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether the composition is a pop-up menu, shown only on request (as
    /// over a film), not when its playlist starts.
    pub fn is_pop_up(&self) -> bool {
        // interactive_composition_length 24, then stream_model 1 |
        // user_interface_model 1.
        self.head.is_some() && self.composition.get(3).is_some_and(|m| m & 0x40 != 0)
    }

    /// Takes the segments of one PES packet's payload. Returns false once
    /// the display set is complete.
    pub fn push(&mut self, mut payload: &[u8]) -> bool {
        while !self.done && payload.len() >= 3 {
            let kind = payload[0];
            let len = usize::from(u16::from_be_bytes([payload[1], payload[2]]));
            let Some(body) = payload.get(3..3 + len) else {
                break; // cut off
            };
            payload = &payload[3 + len..];
            if self.kept + len > MAX_BYTES {
                self.done = true;
                break;
            }
            self.segment(kind, body);
        }
        !self.done
    }

    fn segment(&mut self, kind: u8, body: &[u8]) {
        match kind {
            ICS => self.ics(body),
            // Palettes and objects before the first ICS belong to no display
            // set (libbluray drops them too).
            PDS if self.head.is_some() => self.pds(body),
            ODS if self.head.is_some() => self.ods(body),
            END if self.head.is_some() => self.done = true,
            _ => {}
        }
    }

    fn ics(&mut self, body: &[u8]) {
        if body.len() < 9 {
            return;
        }
        let (first, last) = first_last(body[8]);
        match self.head {
            None if first => {
                let mut head = [0u8; 8];
                head.copy_from_slice(&body[..8]);
                self.head = Some(head);
                self.composition = body[9..].to_vec();
                self.composition_complete = last;
            }
            // The next part of the same composition (same number and state).
            Some(head) if !self.composition_complete && !first && head[5..8] == body[5..8] => {
                self.composition.extend_from_slice(&body[9..]);
                self.composition_complete = last;
            }
            None => {}
            // A new composition: the display set ended without its end
            // segment.
            Some(_) => self.done = true,
        }
        self.kept += body.len();
    }

    fn pds(&mut self, body: &[u8]) {
        let Some((&id, rest)) = body.split_first() else {
            return;
        };
        // palette_version 8, then entries of id, Y, Cr, Cb, alpha.
        let entries = rest.get(1..).unwrap_or_default();
        let palette = match self.palettes.iter_mut().find(|(p, _)| *p == id) {
            Some((_, p)) => p,
            None => {
                self.palettes.push((id, [[0; 4]; 256]));
                &mut self.palettes.last_mut().expect("just pushed").1
            }
        };
        for e in entries.chunks_exact(5) {
            palette[usize::from(e[0])] = [e[1], e[2], e[3], e[4]];
        }
        self.kept += body.len();
    }

    fn ods(&mut self, body: &[u8]) {
        if body.len() < 4 {
            return;
        }
        let id = u16::from_be_bytes([body[0], body[1]]);
        let (first, last) = first_last(body[3]);
        let data = &body[4..];
        match self.index.get(&id) {
            // A new version of an object replaces the old one.
            Some(&i) if first => {
                let o = &mut self.objects[i];
                o.data.clear();
                o.data.extend_from_slice(data);
                o.complete = last;
            }
            Some(&i) => {
                let o = &mut self.objects[i];
                if !o.complete {
                    o.data.extend_from_slice(data);
                    o.complete = last;
                }
            }
            None if first => {
                if self.objects.len() == MAX_OBJECTS {
                    self.done = true;
                    return;
                }
                self.index.insert(id, self.objects.len());
                self.objects.push(Object {
                    data: data.to_vec(),
                    complete: last,
                });
            }
            None => {}
        }
        self.kept += body.len();
    }

    /// What the first page of the composition shows, or `None` when there is
    /// no usable composition or it is a pop-up menu. `selected` is the button
    /// the player was told to select (PSR 10, set by `SetButtonPage`), used
    /// when the page names no default. Objects not complete are left out.
    pub fn into_menu(mut self, selected: Option<u16>) -> Option<Menu> {
        let head = self.head?;
        let width = u16::from_be_bytes([head[0], head[1]]);
        let height = u16::from_be_bytes([head[2], head[3]]);
        if width == 0 || height == 0 || width > MAX_PLANE_WIDTH || height > MAX_PLANE_HEIGHT {
            return None;
        }
        let page = first_page(&self.composition, selected)?;
        let palette = self
            .palettes
            .iter()
            .find(|(id, _)| *id == page.palette)
            .map(|(_, p)| Box::new(*p))?;
        // The objects drawn, each kept once however many buttons show it.
        let mut objects: Vec<Vec<u8>> = Vec::new();
        let mut kept: HashMap<u16, usize> = HashMap::new();
        let mut draws = Vec::new();
        for &(x, y, ids) in page.shown.iter().take(MAX_DRAWS) {
            let usable = |id: &u16| {
                kept.contains_key(id)
                    || self
                        .index
                        .get(id)
                        .is_some_and(|&i| self.objects[i].complete)
            };
            let Some(id) = ids.into_iter().find(usable) else {
                continue;
            };
            let object = match kept.get(&id) {
                Some(&k) => k,
                None => {
                    let data = std::mem::take(&mut self.objects[self.index[&id]].data);
                    objects.push(data);
                    kept.insert(id, objects.len() - 1);
                    objects.len() - 1
                }
            };
            draws.push(Draw { x, y, object });
        }
        Some(Menu {
            width,
            height,
            palette,
            objects,
            draws,
        })
    }
}

/// Reads big-endian fields one after another; `None` past the end.
struct Fields<'a> {
    d: &'a [u8],
    at: usize,
}

impl Fields<'_> {
    fn skip(&mut self, n: usize) -> Option<()> {
        self.at = self.at.checked_add(n).filter(|&e| e <= self.d.len())?;
        Some(())
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.at)?;
        self.at += 1;
        Some(v)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }

    fn u24(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes([0, self.u8()?, self.u8()?, self.u8()?]))
    }

    /// An effect sequence: windows and effects with their composition
    /// objects; only skipped.
    fn effect_sequence(&mut self) -> Option<()> {
        let windows = self.u8()?;
        self.skip(9 * usize::from(windows))?;
        for _ in 0..self.u8()? {
            // duration 24, palette_id_ref 8, number_of_composition_objects 8.
            self.skip(4)?;
            for _ in 0..self.u8()? {
                // object_id_ref 16, window_id_ref 8, cropped 1 | forced 1 |
                // reserved 6, position 2 x 16, cropping rectangle when cropped.
                self.skip(3)?;
                let cropped = self.u8()? & 0x80 != 0;
                self.skip(if cropped { 12 } else { 4 })?;
            }
        }
        Some(())
    }
}

/// What a page shows.
struct Page {
    palette: u8,
    /// Position of each button shown, and its object: the one it settles
    /// on, else the first of its animation.
    shown: Vec<(u16, u16, [u16; 2])>,
}

/// The objects of one state of a button.
#[derive(Clone, Copy)]
struct State {
    start: u16,
    end: u16,
    repeat: bool,
}

impl State {
    /// The object shown once the menu has settled, then the first one: an
    /// animation (start before end) that plays once stays on its end object,
    /// one that repeats keeps cycling and is shown at its start, and a state
    /// that does not animate shows its start (libbluray
    /// `_find_object_for_button`).
    fn settled(self) -> [u16; 2] {
        let animates = self.end > self.start && self.end < NONE;
        if animates && !self.repeat && self.end < LAST_OBJECT {
            [self.end, self.start]
        } else {
            [self.start, self.start]
        }
    }
}

#[derive(Clone, Copy)]
struct Button {
    id: u16,
    x: u16,
    y: u16,
    normal: State,
    selected: State,
}

/// Reads the page with id 0 (the first shown) of an interactive composition,
/// or the first page when none has id 0. `None` for a pop-up menu.
/// `selected` as for `Collector::into_menu`.
fn first_page(d: &[u8], selected: Option<u16>) -> Option<Page> {
    let mut f = Fields { d, at: 0 };
    // interactive_composition_length 24 (what follows), stream_model 1 |
    // user_interface_model 1 | reserved 6.
    f.u24()?;
    let models = f.u8()?;
    if models & 0x40 != 0 {
        return None; // pop-up
    }
    if models & 0x80 == 0 {
        // Multiplexed with the video: composition and selection time-outs.
        f.skip(10)?;
    }
    // user_time_out_duration 24, number_of_pages 8.
    f.skip(3)?;
    let pages = f.u8()?;
    let mut chosen = None;
    for _ in 0..pages {
        let page = page(&mut f, selected)?;
        let is_zero = page.0 == 0;
        if chosen.is_none() || is_zero {
            chosen = Some(page.1);
        }
        if is_zero {
            break;
        }
    }
    chosen
}

/// Reads one page: (page_id, what it shows).
fn page(f: &mut Fields, hint: Option<u16>) -> Option<(u8, Page)> {
    let id = f.u8()?;
    // page_version_number 8, UO_mask_table 64.
    f.skip(9)?;
    f.effect_sequence()?; // in effects
    f.effect_sequence()?; // out effects
                          // animation_frame_rate_code 8.
    f.skip(1)?;
    let default_selected = f.u16()?;
    f.skip(2)?; // default_activated_button_id_ref
    let palette = f.u8()?;
    let bogs = f.u8()?;
    // The button shown of each BOG: the first with its valid id.
    let mut buttons: Vec<Button> = Vec::new();
    for _ in 0..bogs {
        let valid = f.u16()?;
        let mut found = false;
        for _ in 0..f.u8()? {
            let b = button(f)?;
            if !found && b.id == valid && valid != NONE {
                buttons.push(b);
                found = true;
            }
        }
    }
    // The selected button (libbluray `_find_selected_button_id`): the
    // default if shown, else the one selected before (PSR 10) if shown, else
    // the first shown.
    let shown_id = |id: u16| buttons.iter().find(|b| b.id == id).map(|b| b.id);
    let selected = shown_id(default_selected)
        .or_else(|| hint.and_then(shown_id))
        .or(buttons.first().map(|b| b.id));
    let shown = buttons
        .iter()
        .map(|b| {
            let state = if Some(b.id) == selected {
                b.selected
            } else {
                b.normal
            };
            (b.x, b.y, state.settled())
        })
        .filter(|&(_, _, objects)| objects[0] != NONE || objects[1] != NONE)
        .collect();
    Some((id, Page { palette, shown }))
}

fn button(f: &mut Fields) -> Option<Button> {
    let id = f.u16()?;
    // numeric_select_value 16, auto_action 1 | reserved 7.
    f.skip(3)?;
    let x = f.u16()?;
    let y = f.u16()?;
    // Neighbour buttons: upper, lower, left, right.
    f.skip(8)?;
    // Start and end object, repeat 1 | reserved 7.
    let state = |f: &mut Fields| -> Option<State> {
        Some(State {
            start: f.u16()?,
            end: f.u16()?,
            repeat: f.u8()? & 0x80 != 0,
        })
    };
    let normal = state(f)?;
    f.skip(1)?; // selected sound
    let selected = state(f)?;
    // Activated sound 8, activated start and end 2 x 16.
    f.skip(5)?;
    let commands = f.u16()?;
    f.skip(12 * usize::from(commands))?;
    Some(Button {
        id,
        x,
        y,
        normal,
        selected,
    })
}

/// An object (by index in `Menu::objects`) to draw at a position of the
/// graphics plane.
struct Draw {
    x: u16,
    y: u16,
    object: usize,
}

/// What a menu shows over its video.
pub struct Menu {
    /// Size of the graphics plane.
    width: u16,
    height: u16,
    palette: Box<Palette>,
    /// object_data of the objects shown.
    objects: Vec<Vec<u8>>,
    draws: Vec<Draw>,
}

/// Converts studio-range Y'CbCr between colour matrices.
fn convert(y: u8, cb: u8, cr: u8, from: ColorMatrix, to: ColorMatrix) -> (u8, u8, u8) {
    if from == to {
        return (y, cb, cr);
    }
    let k = |m| match m {
        ColorMatrix::Bt601 => (0.299f32, 0.114f32),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
    };
    let (kr, kb) = k(from);
    let (ey, epb, epr) = (
        f32::from(y) - 16.0,
        f32::from(cb) - 128.0,
        f32::from(cr) - 128.0,
    );
    // R', G', B' scaled like Y' (219 levels).
    let r = ey + epr * (1.0 - kr) * 2.0 * 219.0 / 224.0;
    let b = ey + epb * (1.0 - kb) * 2.0 * 219.0 / 224.0;
    let g = (ey - kr * r - kb * b) / (1.0 - kr - kb);
    let (kr, kb) = k(to);
    let ny = kr * r + (1.0 - kr - kb) * g + kb * b;
    let ncb = (b - ny) / (2.0 * (1.0 - kb)) * 224.0 / 219.0;
    let ncr = (r - ny) / (2.0 * (1.0 - kr)) * 224.0 / 219.0;
    let q = |v: f32, offset: f32| (v + offset).round().clamp(0.0, 255.0) as u8;
    (q(ny, 16.0), q(ncb, 128.0), q(ncr, 128.0))
}

impl Menu {
    /// Whether the menu draws anything at all.
    pub fn is_empty(&self) -> bool {
        self.draws.is_empty()
    }

    /// The graphics plane, `width` x `height` samples of (Y, Cb, Cr, alpha)
    /// in the colours of `matrix`.
    fn plane(&self, matrix: ColorMatrix) -> Vec<[u8; 4]> {
        let (pw, ph) = (usize::from(self.width), usize::from(self.height));
        // Menus of HD video have BT.709 palettes, those of SD video BT.601.
        let own = if self.height > 576 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt601
        };
        let mut palette = *self.palette;
        for e in palette.iter_mut() {
            let (y, cb, cr) = convert(e[0], e[2], e[1], own, matrix);
            *e = [y, cr, cb, e[3]];
        }
        let mut plane = vec![[0u8; 4]; pw * ph];
        let mut budget = MAX_PIXELS_PER_PLANE * pw * ph;
        for d in &self.draws {
            let data = &self.objects[d.object];
            draw_object(
                &mut plane,
                (pw, ph),
                (d.x, d.y),
                data,
                &palette,
                &mut budget,
            );
        }
        plane
    }

    /// Draws the menu over `frame`, scaled to it.
    pub fn draw(&self, frame: &mut Frame) {
        let (pw, ph) = (usize::from(self.width), usize::from(self.height));
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        let (cw, ch) = (
            frame.chroma_width() as usize,
            frame.chroma_height() as usize,
        );
        if self.draws.is_empty()
            || fw == 0
            || fh == 0
            || frame.y.len() < fw * fh
            || frame.cb.len() < cw * ch
            || frame.cr.len() < cw * ch
        {
            return;
        }
        let plane = self.plane(frame.matrix);
        // Each frame sample shows the plane sample at its place.
        let xs: Vec<usize> = (0..fw).map(|x| x * pw / fw).collect();
        let ys: Vec<usize> = (0..fh).map(|y| y * ph / fh).collect();
        for (y, &py) in ys.iter().enumerate() {
            let row = &plane[py * pw..(py + 1) * pw];
            for (x, &px) in xs.iter().enumerate() {
                let [gy, _, _, a] = row[px];
                if a > 0 {
                    let v = &mut frame.y[y * fw + x];
                    *v = blend(*v, gy, u32::from(a));
                }
            }
        }
        // A chroma sample covers 2 x 2 frame samples: blended with the
        // plane's colour weighted by its coverage.
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut a, mut cb, mut cr) = (0u32, 0u32, 0u32);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (x, y) = ((2 * cx + dx).min(fw - 1), (2 * cy + dy).min(fh - 1));
                    let p = plane[ys[y] * pw + xs[x]];
                    let pa = u32::from(p[3]);
                    a += pa;
                    cb += pa * u32::from(p[1]);
                    cr += pa * u32::from(p[2]);
                }
                if a == 0 {
                    continue;
                }
                let i = cy * cw + cx;
                let keep = 4 * 255 - a;
                frame.cb[i] = ((cb + keep * u32::from(frame.cb[i]) + 510) / 1020) as u8;
                frame.cr[i] = ((cr + keep * u32::from(frame.cr[i]) + 510) / 1020) as u8;
            }
        }
    }
}

/// `over` with alpha `a` (0..=255).
fn blend(under: u8, over: u8, a: u32) -> u8 {
    ((u32::from(over) * a + u32::from(under) * (255 - a) + 127) / 255) as u8
}

/// Decodes an object's run-length coded lines onto the plane of size
/// (`pw`, `ph`) at (`ox`, `oy`). Every run costs one unit of `budget` and
/// one for each pixel drawn; the object is left unfinished once it is spent.
fn draw_object(
    plane: &mut [[u8; 4]],
    (pw, ph): (usize, usize),
    (ox, oy): (u16, u16),
    data: &[u8],
    palette: &Palette,
    budget: &mut usize,
) {
    // object_data_length 24, object_width 16, object_height 16.
    let Some(h) = data.get(..7) else {
        return;
    };
    let length = u32::from_be_bytes([0, h[0], h[1], h[2]]) as usize;
    let (w, oh) = (
        usize::from(u16::from_be_bytes([h[3], h[4]])),
        usize::from(u16::from_be_bytes([h[5], h[6]])),
    );
    // The length counts the size fields and the coded lines.
    let end = (3 + length).min(data.len());
    let rle = data.get(7..end).unwrap_or_default();
    let (ox, oy) = (usize::from(ox), usize::from(oy));
    let (mut x, mut y, mut i) = (0usize, 0usize, 0usize);
    while y < oh && i < rle.len() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let b = rle[i];
        i += 1;
        let (run, colour) = if b != 0 {
            (1, b)
        } else {
            // 00 00 ends the line; 00 then 0L.., 01L.., 10L.. C, 11L.. C: a
            // run of 6 or 14 bits of colour 0 or C.
            let Some(&flags) = rle.get(i) else { break };
            i += 1;
            if flags == 0 {
                x = 0;
                y += 1;
                continue;
            }
            let mut run = usize::from(flags & 0x3F);
            if flags & 0x40 != 0 {
                let Some(&low) = rle.get(i) else { break };
                i += 1;
                run = (run << 8) | usize::from(low);
            }
            let colour = if flags & 0x80 != 0 {
                let Some(&c) = rle.get(i) else { break };
                i += 1;
                c
            } else {
                0
            };
            (run, colour)
        };
        let entry = palette[usize::from(colour)];
        let (py, from) = (oy + y, ox + x);
        let to = (ox + (x + run).min(w)).min(pw);
        x += run;
        if entry[3] == 0 || py >= ph || from >= to {
            continue;
        }
        let n = to - from;
        if n > *budget {
            return;
        }
        *budget -= n;
        let colour = [entry[0], entry[2], entry[1], entry[3]];
        for p in &mut plane[py * pw + from..py * pw + to] {
            *p = over(*p, colour);
        }
    }
}

/// Puts `top` (Y, Cb, Cr, alpha) over a plane sample.
fn over(under: [u8; 4], top: [u8; 4]) -> [u8; 4] {
    let (ta, ua) = (u32::from(top[3]), u32::from(under[3]));
    if ta == 255 || ua == 0 {
        return top;
    }
    // Resulting alpha and colours, in 255ths.
    let below = ua * (255 - ta) / 255;
    let a = ta + below;
    let mix = |t: u8, u: u8| ((u32::from(t) * ta + u32::from(u) * below + a / 2) / a) as u8;
    [
        mix(top[0], under[0]),
        mix(top[1], under[1]),
        mix(top[2], under[2]),
        a as u8,
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Run-length codes `lines` of palette indices.
    pub(crate) fn rle(lines: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            let mut i = 0;
            while i < line.len() {
                let c = line[i];
                let mut n = 1;
                while i + n < line.len() && line[i + n] == c && n < 0x3FFF {
                    n += 1;
                }
                match (c, n) {
                    (c, 1) if c != 0 => out.push(c),
                    (c, 2) if c != 0 => out.extend([c, c]),
                    (0, n) if n < 64 => out.extend([0, n as u8]),
                    (0, n) => out.extend([0, 0x40 | (n >> 8) as u8, n as u8]),
                    (c, n) if n < 64 => out.extend([0, 0x80 | n as u8, c]),
                    (c, n) => out.extend([0, 0xC0 | (n >> 8) as u8, n as u8, c]),
                }
                i += n;
            }
            out.extend([0, 0]);
        }
        out
    }

    fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![kind];
        s.extend((body.len() as u16).to_be_bytes());
        s.extend(body);
        s
    }

    /// ODS segments of object `id` (`lines` of `w` pixels), split into parts
    /// of at most `part` coded bytes.
    pub(crate) fn ods(id: u16, w: u16, lines: &[Vec<u8>], part: usize) -> Vec<Vec<u8>> {
        let coded = rle(lines);
        let mut data = ((coded.len() + 4) as u32).to_be_bytes()[1..].to_vec();
        data.extend(w.to_be_bytes());
        data.extend((lines.len() as u16).to_be_bytes());
        data.extend(coded);
        let chunks: Vec<&[u8]> = data.chunks(part).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut body = id.to_be_bytes().to_vec();
                body.push(0);
                body.push(
                    if i == 0 { 0x80 } else { 0 } | if i + 1 == chunks.len() { 0x40 } else { 0 },
                );
                body.extend(*c);
                segment(ODS, &body)
            })
            .collect()
    }

    /// A PDS of `entries` (index, Y, Cr, Cb, alpha).
    pub(crate) fn pds(id: u8, entries: &[[u8; 5]]) -> Vec<u8> {
        let mut body = vec![id, 0];
        for e in entries {
            body.extend(e);
        }
        segment(PDS, &body)
    }

    pub(crate) fn end() -> Vec<u8> {
        segment(END, &[])
    }

    /// A button of one object per state: (id, x, y, normal object,
    /// selected object).
    pub(crate) type B = (u16, u16, u16, u16, u16);
    /// The objects of a state: (start, end, repeat).
    type S = (u16, u16, bool);

    pub(crate) fn button(b: B, commands: usize) -> Vec<u8> {
        let (id, x, y, normal, selected) = b;
        animated(
            id,
            (x, y),
            (normal, normal, false),
            (selected, selected, false),
            commands,
        )
    }

    /// A button with animated states.
    fn animated(id: u16, (x, y): (u16, u16), normal: S, selected: S, commands: usize) -> Vec<u8> {
        let mut d = id.to_be_bytes().to_vec();
        d.extend([0, 0, 0]);
        d.extend(x.to_be_bytes());
        d.extend(y.to_be_bytes());
        d.extend([0xFFu8; 8]);
        for (i, (start, end, repeat)) in [normal, selected].into_iter().enumerate() {
            if i == 1 {
                d.push(0xFF); // selected sound
            }
            d.extend(start.to_be_bytes());
            d.extend(end.to_be_bytes());
            d.push(if repeat { 0x80 } else { 0 });
        }
        d.extend([0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        d.extend((commands as u16).to_be_bytes());
        for _ in 0..commands {
            d.extend([0x22, 0x80, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        }
        d
    }

    /// A page: its id, default selected button, palette and BOGs as (valid
    /// button, buttons); `effects` adds an in effect.
    pub(crate) fn page(
        id: u8,
        selected: u16,
        palette: u8,
        bogs: &[(u16, Vec<B>)],
        effects: bool,
    ) -> Vec<u8> {
        let bogs: Vec<(u16, Vec<Vec<u8>>)> = bogs
            .iter()
            .map(|(valid, buttons)| {
                let coded = buttons
                    .iter()
                    .enumerate()
                    .map(|(i, b)| button(*b, i % 3))
                    .collect();
                (*valid, coded)
            })
            .collect();
        page_of(id, selected, palette, &bogs, effects)
    }

    /// A page of buttons already coded.
    fn page_of(
        id: u8,
        selected: u16,
        palette: u8,
        bogs: &[(u16, Vec<Vec<u8>>)],
        effects: bool,
    ) -> Vec<u8> {
        let mut d = vec![id, 0];
        d.extend([0u8; 8]);
        if effects {
            // One window, one effect with a cropped and a plain object.
            d.push(1);
            d.extend([0, 0, 0, 0, 0, 0x07, 0x80, 0x04, 0x38]);
            d.extend([1, 0, 0, 10, 0, 2]);
            d.extend([0, 1, 0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 0, 10]);
            d.extend([0, 2, 0, 0x00, 0, 5, 0, 5]);
        } else {
            d.push(0);
            d.push(0);
        }
        d.extend([0, 0]); // out effects: none
        d.push(0x20);
        d.extend(selected.to_be_bytes());
        d.extend(NONE.to_be_bytes());
        d.push(palette);
        d.push(bogs.len() as u8);
        for (valid, buttons) in bogs {
            d.extend(valid.to_be_bytes());
            d.push(buttons.len() as u8);
            for b in buttons {
                d.extend(b);
            }
        }
        d
    }

    /// ICS segments for a plane of `w` x `h` with `pages`, multiplexed with
    /// the video, always on or `popup`, split into parts of at most `part`
    /// bytes of composition.
    pub(crate) fn ics(w: u16, h: u16, pages: &[Vec<u8>], popup: bool, part: usize) -> Vec<Vec<u8>> {
        let mut comp = vec![if popup { 0x40 } else { 0x00 }];
        comp.extend([0u8; 10]);
        comp.extend([0, 0, 0]);
        comp.push(pages.len() as u8);
        for p in pages {
            comp.extend(p);
        }
        let mut data = (comp.len() as u32).to_be_bytes()[1..].to_vec();
        data.extend(comp);
        let chunks: Vec<&[u8]> = data.chunks(part).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut body = w.to_be_bytes().to_vec();
                body.extend(h.to_be_bytes());
                body.push(0x10);
                body.extend([0, 1, 0x80]); // composition 1, epoch start
                body.push(
                    if i == 0 { 0x80 } else { 0 } | if i + 1 == chunks.len() { 0x40 } else { 0 },
                );
                body.extend(*c);
                segment(ICS, &body)
            })
            .collect()
    }

    fn collect(segments: &[Vec<u8>]) -> Collector {
        let mut c = Collector::default();
        for s in segments {
            c.push(s);
        }
        c
    }

    /// A 4:2:0 frame of one colour.
    fn frame(w: u32, h: u32, (y, cb, cr): (u8, u8, u8), matrix: ColorMatrix) -> Frame {
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        Frame {
            width: w,
            height: h,
            y: vec![y; (w * h) as usize],
            cb: vec![cb; cw * ch],
            cr: vec![cr; cw * ch],
            pixel_aspect: (1, 1),
            matrix,
            field_doubled: false,
            mpeg1: false,
            concealed_macroblocks: 0,
            total_macroblocks: 1,
        }
    }

    /// Two buttons in one BOG each, the second selected by default; colour 1
    /// is opaque red, 2 half-transparent white, 3 opaque green.
    fn two_buttons() -> Vec<Vec<u8>> {
        let mut s = ics(
            64,
            32,
            &[page(
                0,
                2,
                7,
                &[
                    (1, vec![(1, 4, 4, 10, 11)]),
                    (2, vec![(9, 0, 0, 10, 10), (2, 40, 8, 12, 13)]),
                ],
                true,
            )],
            false,
            1000,
        );
        s.push(pds(
            7,
            &[
                [1, 81, 240, 90, 255],
                [2, 235, 128, 128, 128],
                [3, 145, 34, 54, 255],
            ],
        ));
        s.extend(ods(10, 8, &vec![vec![1; 8]; 4], 1000));
        s.extend(ods(11, 8, &vec![vec![2; 8]; 4], 1000));
        s.extend(ods(13, 6, &vec![vec![3, 3, 0, 0, 3, 3]; 6], 1000));
        s.push(end());
        s
    }

    #[test]
    fn draws_the_buttons_of_the_first_page() {
        let c = collect(&two_buttons());
        assert!(c.done());
        let menu = c.into_menu(None).unwrap();
        // A plane of SD size has BT.601 colours, as the frame.
        let mut f = frame(64, 32, (16, 128, 128), ColorMatrix::Bt601);
        menu.draw(&mut f);
        let y = |x: usize, y: usize| f.y[y * 64 + x];
        // Button 1, normal: red over black.
        assert_eq!((y(4, 4), y(11, 7)), (81, 81));
        assert_eq!(f.cr[2 * 32 + 2], 240);
        assert_eq!((y(3, 4), y(12, 4), y(4, 8)), (16, 16, 16));
        // Button 9 shares BOG 2 with the shown button 2: not drawn.
        assert_eq!((y(0, 0), y(7, 3), f.cr[0]), (16, 16, 128));
        // Button 2, selected: green with a transparent gap.
        assert_eq!(
            (y(40, 8), y(41, 13), y(42, 8), y(44, 8)),
            (145, 145, 16, 145)
        );
        // The rest is untouched.
        assert_eq!(y(30, 20), 16);
        assert_eq!(f.cb[10 * 32 + 15], 128);
    }

    #[test]
    fn half_transparent_colours_blend() {
        let mut s = ics(
            16,
            16,
            &[page(0, 1, 0, &[(1, vec![(1, 0, 0, 5, 5)])], false)],
            false,
            1000,
        );
        s.push(pds(0, &[[1, 235, 128, 128, 128]]));
        s.extend(ods(5, 16, &vec![vec![1; 16]; 16], 1000));
        s.push(end());
        let menu = collect(&s).into_menu(None).unwrap();
        let mut f = frame(16, 16, (16, 128, 128), ColorMatrix::Bt709);
        menu.draw(&mut f);
        assert!(f.y.iter().all(|&v| v == 126), "{:?}", &f.y[..4]);
    }

    #[test]
    fn split_compositions_and_objects_are_joined() {
        let mut s = ics(
            64,
            32,
            &[page(0, 0xFFFF, 1, &[(4, vec![(4, 10, 10, 20, 20)])], true)],
            false,
            7,
        );
        s.push(pds(1, &[[9, 200, 128, 128, 255]]));
        let lines: Vec<Vec<u8>> = (0..12)
            .map(|i| {
                (0..30)
                    .map(|x| if (x + i) % 3 == 0 { 9 } else { 0 })
                    .collect()
            })
            .collect();
        s.extend(ods(20, 30, &lines, 13));
        s.push(end());
        assert!(s.len() > 10);
        let menu = collect(&s).into_menu(None).unwrap();
        let mut f = frame(64, 32, (16, 128, 128), ColorMatrix::Bt709);
        menu.draw(&mut f);
        for (i, line) in lines.iter().enumerate() {
            for (x, &c) in line.iter().enumerate() {
                let want = if c == 9 { 200 } else { 16 };
                assert_eq!(f.y[(10 + i) * 64 + 10 + x], want, "({x}, {i})");
            }
        }
    }

    #[test]
    fn segments_may_share_a_packet_and_long_runs_are_coded() {
        let mut packet = Vec::new();
        for s in ics(
            1920,
            1080,
            &[page(0, 1, 0, &[(1, vec![(1, 0, 1000, 3, 3)])], false)],
            false,
            1000,
        ) {
            packet.extend(s);
        }
        packet.extend(pds(0, &[[1, 235, 128, 128, 255]]));
        for s in ods(
            3,
            1920,
            &[vec![1; 1920], vec![0; 1920], vec![1; 1000]],
            60000,
        ) {
            packet.extend(s);
        }
        packet.extend(end());
        let mut c = Collector::default();
        assert!(!c.push(&packet));
        let menu = c.into_menu(None).unwrap();
        // Scaled down to a quarter.
        let mut f = frame(480, 270, (16, 128, 128), ColorMatrix::Bt709);
        menu.draw(&mut f);
        assert_eq!(f.y[250 * 480 + 479], 235);
        assert_eq!(f.y[249 * 480], 16);
        assert_eq!(f.y[251 * 480 + 200], 16);
    }

    #[test]
    fn pop_up_menus_and_missing_parts_give_nothing() {
        let page0 = page(0, 1, 0, &[(1, vec![(1, 0, 0, 3, 3)])], false);
        let mut s = ics(64, 32, &[page0.clone()], true, 1000);
        s.push(pds(0, &[[1, 235, 128, 128, 255]]));
        assert!(collect(&s).is_pop_up());
        assert!(collect(&s).into_menu(None).is_none());
        assert!(!collect(&ics(64, 32, &[page0.clone()], false, 1000)).is_pop_up());
        // No palette of the page's id.
        let mut s = ics(64, 32, &[page0.clone()], false, 1000);
        s.push(pds(5, &[[1, 235, 128, 128, 255]]));
        assert!(collect(&s).into_menu(None).is_none());
        // Segments before the composition belong to nothing.
        let mut s = vec![pds(0, &[[1, 235, 128, 128, 255]])];
        s.extend(ods(3, 4, &[vec![1; 4]], 100));
        s.extend(ics(64, 32, &[page0.clone()], false, 1000));
        s.push(end());
        let c = collect(&s);
        assert!(c.done());
        assert!(c.into_menu(None).is_none());
        // A missing object draws nothing.
        let mut s = ics(64, 32, &[page0], false, 1000);
        s.push(pds(0, &[[1, 235, 128, 128, 255]]));
        assert!(collect(&s).into_menu(None).unwrap().is_empty());
        // Planes larger than Blu-ray's, one side at a time, with everything
        // else in place.
        for (w, h, usable) in [
            (1920, 1080, true),
            (1921, 1080, false),
            (1920, 1081, false),
            (65535, 65535, false),
        ] {
            let mut s = ics(
                w,
                h,
                &[page(0, 1, 0, &[(1, vec![(1, 0, 0, 3, 3)])], false)],
                false,
                1000,
            );
            s.push(pds(0, &[[1, 235, 128, 128, 255]]));
            s.extend(ods(3, 4, &[vec![1; 4]], 100));
            assert_eq!(collect(&s).into_menu(None).is_some(), usable, "{w}x{h}");
        }
    }

    /// A page of two BOGs whose buttons' normal (object 1) and selected
    /// (object 2) states differ, with `default` selected first; the
    /// selected state draws Y 200, the normal one Y 100. Returns the luma
    /// under each button.
    fn selection(default: u16, hint: Option<u16>) -> [u8; 2] {
        let bogs = [(1, vec![(1, 0, 0, 1, 2)]), (2, vec![(2, 8, 0, 1, 2)])];
        let mut s = ics(16, 8, &[page(0, default, 0, &bogs, false)], false, 1000);
        s.push(pds(0, &[[1, 100, 128, 128, 255], [2, 200, 128, 128, 255]]));
        s.extend(ods(1, 4, &vec![vec![1; 4]; 4], 1000));
        s.extend(ods(2, 4, &vec![vec![2; 4]; 4], 1000));
        let menu = collect(&s).into_menu(hint).unwrap();
        let mut f = frame(16, 8, (16, 128, 128), ColorMatrix::Bt601);
        menu.draw(&mut f);
        [f.y[0], f.y[8]]
    }

    #[test]
    fn the_selected_button_follows_libbluray() {
        // The page's default when shown.
        assert_eq!(selection(2, Some(1)), [100, 200]);
        // Else the button selected before (PSR 10) when shown.
        assert_eq!(selection(0xFFFF, Some(2)), [100, 200]);
        assert_eq!(selection(7, Some(2)), [100, 200]);
        // Else the first shown.
        assert_eq!(selection(0xFFFF, None), [200, 100]);
        assert_eq!(selection(7, Some(9)), [200, 100]);
    }

    #[test]
    fn a_bog_shows_its_first_button_of_the_valid_id() {
        // Two buttons with the BOG's valid id: only the first is drawn.
        let bogs = [(5, vec![(5, 0, 0, 1, 1), (5, 8, 0, 1, 1)])];
        let mut s = ics(16, 8, &[page(0, 5, 0, &bogs, false)], false, 1000);
        s.push(pds(0, &[[1, 200, 128, 128, 255]]));
        s.extend(ods(1, 4, &vec![vec![1; 4]; 4], 1000));
        let menu = collect(&s).into_menu(None).unwrap();
        let mut f = frame(16, 8, (16, 128, 128), ColorMatrix::Bt601);
        menu.draw(&mut f);
        assert_eq!([f.y[0], f.y[8]], [200, 16]);
    }

    #[test]
    fn animations_are_shown_as_they_settle() {
        // Objects 1..=3 are Y 100, 150, 200; each button shows one state.
        let draw = |normal: S, selected: S| {
            // Button 1 is not selected, button 2 is.
            let bogs = vec![
                (1, vec![animated(1, (0, 0), normal, normal, 0)]),
                (2, vec![animated(2, (8, 0), selected, selected, 0)]),
            ];
            let mut s = ics(16, 8, &[page_of(0, 2, 0, &bogs, false)], false, 1000);
            s.push(pds(
                0,
                &[
                    [1, 100, 128, 128, 255],
                    [2, 150, 128, 128, 255],
                    [3, 200, 128, 128, 255],
                ],
            ));
            for id in 1..=3u16 {
                s.extend(ods(id, 4, &vec![vec![id as u8; 4]; 4], 1000));
            }
            let menu = collect(&s).into_menu(None).unwrap();
            let mut f = frame(16, 8, (16, 128, 128), ColorMatrix::Bt601);
            menu.draw(&mut f);
            [f.y[0], f.y[8]]
        };
        // An animation played once stays on its end object, in either state.
        assert_eq!(draw((1, 3, false), (1, 3, false)), [200, 200]);
        // One that repeats is shown at its start.
        assert_eq!(draw((1, 3, true), (2, 3, true)), [100, 150]);
        // An end object that is missing gives the start one; 0xFFFE and up
        // name none.
        assert_eq!(draw((2, 9, false), (3, 0xFFFE, false)), [150, 200]);
        // Static buttons.
        assert_eq!(draw((2, 2, false), (3, 3, true)), [150, 200]);
        // An end before the start is no animation: the start is shown.
        assert_eq!(draw((3, 1, false), (2, 1, true)), [200, 150]);
    }

    #[test]
    fn colours_are_converted_when_the_matrices_differ() {
        // Red of a BT.709 (HD) plane over a BT.601 frame, and of a BT.601
        // (SD) plane over a BT.709 frame.
        for (h, frame_matrix, want) in [
            (720u16, ColorMatrix::Bt601, (81, 90, 240)),
            (480, ColorMatrix::Bt709, (63, 102, 240)),
        ] {
            let red = if h > 576 {
                [1, 63, 240, 102, 255]
            } else {
                [1, 81, 240, 90, 255]
            };
            let mut s = ics(
                16,
                h,
                &[page(0, 1, 0, &[(1, vec![(1, 0, 0, 3, 3)])], false)],
                false,
                1000,
            );
            s.push(pds(0, &[red]));
            s.extend(ods(3, 16, &vec![vec![1; 16]; h as usize], 60000));
            let menu = collect(&s).into_menu(None).unwrap();
            let mut f = frame(16, u32::from(h), (16, 128, 128), frame_matrix);
            menu.draw(&mut f);
            let got = (f.y[0], f.cb[0], f.cr[0]);
            let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
            assert!(
                near(got.0, want.0) && near(got.1, want.1) && near(got.2, want.2),
                "{h}: {got:?}"
            );
        }
    }

    #[test]
    fn crafted_streams_with_many_objects_and_buttons_cost_little() {
        // A page of 255 BOGs of 255 buttons, all with the BOG's valid id,
        // then 590,000 object segments cycling through 4000 ids, each a new
        // version (a linear search per segment would cost minutes).
        let t = std::time::Instant::now();
        let button = animated(
            7,
            (0, 0),
            (0xFFFD, 0xFFFD, false),
            (0xFFFD, 0xFFFD, false),
            0,
        );
        let bogs: Vec<(u16, Vec<Vec<u8>>)> =
            (0..255).map(|_| (7, vec![button.clone(); 255])).collect();
        let page = page_of(0, 7, 0, &bogs, false);
        let mut c = Collector::default();
        for part in ics(1920, 1080, &[page], false, 60000) {
            c.push(&part);
        }
        c.push(&pds(0, &[[1, 235, 128, 128, 255]]));
        let mut payload = Vec::new();
        for round in 0..9u32 {
            for id in 0..65535u32 {
                payload.extend([ODS, 0, 4]);
                payload.extend((((id + round) % 4000) as u16).to_be_bytes());
                payload.extend([0, 0x80]);
                if payload.len() > 65000 {
                    c.push(&payload);
                    payload.clear();
                }
            }
        }
        c.push(&payload);
        let menu = c.into_menu(None).unwrap();
        assert!(menu.is_empty());
        assert!(
            t.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn later_pages_are_found_by_id() {
        let pages = [
            page(3, 1, 0, &[(1, vec![(1, 0, 0, 3, 3)])], false),
            page(0, 1, 0, &[(1, vec![(1, 0, 0, 4, 4)])], true),
        ];
        let mut s = ics(8, 8, &pages, false, 1000);
        s.push(pds(0, &[[1, 100, 128, 128, 255], [2, 200, 128, 128, 255]]));
        s.extend(ods(3, 8, &vec![vec![1; 8]; 8], 1000));
        s.extend(ods(4, 8, &vec![vec![2; 8]; 8], 1000));
        let menu = collect(&s).into_menu(None).unwrap();
        let mut f = frame(8, 8, (16, 128, 128), ColorMatrix::Bt709);
        menu.draw(&mut f);
        assert!(f.y.iter().all(|&v| v == 200));
    }

    #[test]
    fn colours_follow_the_frame_matrix() {
        // Red, green and blue of one matrix are those of the other (within
        // the rounding of 8-bit codes); greys stay as they are.
        let near = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            a.0.abs_diff(b.0) <= 1 && a.1.abs_diff(b.1) <= 1 && a.2.abs_diff(b.2) <= 1
        };
        let (m601, m709) = (ColorMatrix::Bt601, ColorMatrix::Bt709);
        for (c601, c709) in [
            ((81, 90, 240), (63, 102, 240)),
            ((145, 54, 34), (173, 42, 26)),
            ((41, 240, 110), (32, 240, 118)),
        ] {
            let got = convert(c709.0, c709.1, c709.2, m709, m601);
            assert!(near(got, c601), "{c709:?} -> {got:?}");
            let got = convert(c601.0, c601.1, c601.2, m601, m709);
            assert!(near(got, c709), "{c601:?} -> {got:?}");
        }
        for y in [16, 100, 235] {
            assert_eq!(convert(y, 128, 128, m709, m601), (y, 128, 128));
        }
    }

    fn ffmpeg() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find_map(|dir| {
            ["ffmpeg.exe", "ffmpeg"]
                .iter()
                .map(|name| dir.join(name))
                .find(|p| p.is_file())
        })
    }

    /// Palettes and objects are coded as in presentation graphics (subtitle)
    /// streams: the same palette and object, written once as a subtitle and
    /// rendered by ffmpeg's decoder over black, must give the plane drawn
    /// here. Skipped when ffmpeg is not on PATH.
    #[test]
    fn matches_ffmpeg_presentation_graphics() {
        let Some(ffmpeg) = ffmpeg() else {
            eprintln!("skipped: ffmpeg not on PATH");
            return;
        };
        const W: u16 = 300;
        const X: u16 = 100;
        const Y: u16 = 200;
        // Every run code: single pixels, pairs, short and long runs of
        // colour 0 and of a colour.
        let rows: Vec<Vec<u8>> = (0..40usize)
            .map(|r| match r % 8 {
                0 => [1, 2, 3, 4, 5].repeat(60),
                1 => [&[2, 2, 0, 0, 3, 3, 0].repeat(42)[..], &[4; 6]].concat(),
                2 => [vec![0; 70], vec![1; 70], vec![0; 5], vec![5; 155]].concat(),
                3 => vec![3; 300],
                4 => vec![0; 300],
                5 => [
                    vec![4; 63],
                    vec![5; 64],
                    vec![1],
                    vec![0; 63],
                    vec![2; 64],
                    vec![0; 45],
                ]
                .concat(),
                6 => [vec![1; 100], vec![0; 200]].concat(),
                _ => (0..300).map(|i| ((i * 7 + r) % 6) as u8).collect(),
            })
            .collect();
        // (index, Y, Cr, Cb, alpha): distinct Cb and Cr, one half transparent.
        let palette = [
            [1, 81, 240, 90, 255],
            [2, 145, 34, 54, 255],
            [3, 41, 110, 240, 255],
            [4, 200, 100, 180, 255],
            [5, 120, 160, 70, 128],
        ];
        let objects = ods(1, W, &rows, 60000);
        // The subtitle: composition (object 1 at X, Y), window, palette,
        // object, end; then an empty composition a second later.
        let mut pcs = vec![
            0x07, 0x80, 0x04, 0x38, 0x10, 0, 0, 0x80, 0, 0, 1, 0, 1, 0, 0,
        ];
        pcs.extend(X.to_be_bytes());
        pcs.extend(Y.to_be_bytes());
        let mut wds = vec![1, 0];
        for v in [X, Y, W, 40] {
            wds.extend(v.to_be_bytes());
        }
        let mut sup = Vec::new();
        let mut put = |pts: u32, segments: Vec<Vec<u8>>| {
            for s in segments {
                sup.extend(b"PG");
                sup.extend(pts.to_be_bytes());
                sup.extend(pts.to_be_bytes());
                sup.extend(s);
            }
        };
        let mut first = vec![segment(0x16, &pcs), segment(0x17, &wds), pds(0, &palette)];
        first.extend(objects.iter().cloned());
        first.push(end());
        put(0, first);
        let clear = vec![0x07, 0x80, 0x04, 0x38, 0x10, 0, 1, 0, 0, 0, 0];
        put(
            90_000,
            vec![segment(0x16, &clear), segment(0x17, &[0]), end()],
        );
        let dir = std::env::temp_dir().join(format!("isopreview-igs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sup_path, rgb_path) = (dir.join("pg.sup"), dir.join("pg.rgb"));
        std::fs::write(&sup_path, &sup).unwrap();
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("color=c=black:s=1920x1080:d=0.8:r=25,format=gbrp")
            .arg("-i")
            .arg(&sup_path)
            .args([
                "-filter_complex",
                "[0:v][1:s]overlay=format=gbrp,select=eq(n\\,10)",
            ])
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24"])
            .arg(&rgb_path)
            .status()
            .unwrap();
        assert!(status.success());
        let rgb = std::fs::read(&rgb_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(rgb.len(), 1920 * 1080 * 3);

        // The same palette and object as an interactive composition.
        let mut s = ics(
            1920,
            1080,
            &[page(0, 1, 0, &[(1, vec![(1, X, Y, 1, 1)])], false)],
            false,
            60000,
        );
        s.push(pds(0, &palette));
        s.extend(objects);
        s.push(end());
        let menu = collect(&s).into_menu(None).unwrap();
        let plane = menu.plane(ColorMatrix::Bt709);
        let mut worst = 0u8;
        let mut drawn = 0;
        for y in usize::from(Y) - 4..usize::from(Y) + 44 {
            for x in usize::from(X) - 4..usize::from(X + W) + 4 {
                let [py, cb, cr, a] = plane[y * 1920 + x];
                // Studio-range BT.709 to RGB, over black.
                let (ey, pb, pr) = (
                    (f64::from(py) - 16.0) / 219.0,
                    (f64::from(cb) - 128.0) / 224.0,
                    (f64::from(cr) - 128.0) / 224.0,
                );
                let r = ey + 1.5748 * pr;
                let b = ey + 1.8556 * pb;
                let g = (ey - 0.2126 * r - 0.0722 * b) / 0.7152;
                let alpha = f64::from(a) / 255.0;
                let ours = [r, g, b].map(|v| (v.clamp(0.0, 1.0) * 255.0 * alpha).round() as u8);
                let theirs = &rgb[(y * 1920 + x) * 3..][..3];
                for c in 0..3 {
                    worst = worst.max(ours[c].abs_diff(theirs[c]));
                }
                assert!(
                    ours.iter().zip(theirs).all(|(o, t)| o.abs_diff(*t) <= 3),
                    "({x}, {y}): ours {ours:?}, ffmpeg {theirs:?}"
                );
                drawn += usize::from(a > 0);
            }
        }
        eprintln!("largest difference {worst}, {drawn} pixels drawn");
        assert!(drawn > 7000);
    }

    #[test]
    fn hostile_streams_never_panic_and_draw_little() {
        let base: Vec<u8> = two_buttons().concat();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        for round in 0..3000 {
            let mut d = base.clone();
            for _ in 0..1 + (seed % 6) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % d.len();
                d[i] = (seed >> 32) as u8;
            }
            let cut = if round % 4 == 0 {
                (seed as usize >> 8) % d.len()
            } else {
                d.len()
            };
            let mut c = Collector::default();
            c.push(&d[..cut]);
            if let Some(menu) = c.into_menu(None) {
                let mut f = frame(
                    64 + (round % 3) as u32,
                    32 + (round % 5) as u32,
                    (16, 128, 128),
                    ColorMatrix::Bt601,
                );
                menu.draw(&mut f);
            }
        }
        // A full-plane object drawn by many buttons stops at the budget.
        let buttons: Vec<B> = (0..200).map(|i| (i, 0, 0, 1, 1)).collect();
        let bogs: Vec<(u16, Vec<B>)> = buttons.iter().map(|b| (b.0, vec![*b])).collect();
        let mut s = ics(1920, 1080, &[page(0, 0, 0, &bogs[..], false)], false, 60000);
        s.push(pds(0, &[[1, 100, 128, 128, 200]]));
        s.extend(ods(1, 1920, &vec![vec![1; 1920]; 1080], 60000));
        let menu = collect(&s).into_menu(None).unwrap();
        let mut f = frame(1920, 1080, (16, 128, 128), ColorMatrix::Bt709);
        let t = std::time::Instant::now();
        menu.draw(&mut f);
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }
}
