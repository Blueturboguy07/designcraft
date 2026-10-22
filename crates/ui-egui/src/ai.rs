//! AI tools through publik API or the person's own key, desktop only. The setup flow, the
//! credential file and the HTTP calls live in the `craft-publik` crate (shared by the Craft apps).
//! This module is DesignCraft's thin host: the first-launch dialog, Preferences › AI, and
//! Object › AI › Draft Copy and Generate Image.
//!
//! Nothing is sent, and nothing is charged, until the person clicks Write copy or Generate image
//! in one of the two tool dialogs. The document changes only through commands (`story.setText`,
//! `file.place`), so each change is one undo step.

use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use craft_publik::{Line, Provider, WHY_IT_COSTS};
use designcraft_doc::{Content, Document, ItemId, SpreadRef, StoryId};
use egui::{RichText, Ui};
use serde_json::{Value, json};

use crate::DesignApp;
use crate::dialogs::{AI_DRAFT_COPY, AI_GENERATE_IMAGE, AI_SETUP, Dialog};

const COPY_SYSTEM: &str = "You write copy for a text frame in a page layout. Reply with the copy only: no title unless the brief asks for one, no quotation marks around it, no markdown, no notes. Put each paragraph on its own line, with no blank lines between paragraphs. Write in the language of the brief unless the brief names another language. Stay within the length limit, so that the copy fits the frame.";

/// The sample that measures how much text a frame holds (in its current type style).
const FIT_SAMPLE: &str = "Good copy is short and clear. It says one thing well, and the reader sees why it matters. ";
/// The most text the measure sets; a frame that holds more is "at least" this.
const FIT_CAP: usize = 4000;
/// Characters per word for the word estimate (English averages five letters and a space).
const CHARS_PER_WORD: usize = 6;
const OFF: &str = "AI tools are off in this session (DESIGNCRAFT_NO_PREFS).";

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// The first-launch dialog is due; it opens when no other dialog shows.
    setup_due: bool,
    /// Preferences › AI shows the setup flow in place of the status.
    inline_setup: bool,
    copy: Job,
    /// The story that Fill frame replaces, chosen at the Write copy click: (document uid, story).
    copy_target: Option<(u64, StoryId)>,
    /// How much text the target frame holds, for one document revision.
    fit: Option<(FitKey, Fit)>,
    image: Job,
    /// Where the generated image goes, chosen at the Generate image click.
    image_target: Option<ImageTarget>,
    shape: Shape,
    /// A generated image that is not on the page yet. It was paid for, so it is never dropped.
    unplaced: Option<Vec<u8>>,
}

/// One tool's request in flight and its last answer.
#[derive(Default)]
struct Job {
    prompt: String,
    result: Option<String>,
    error: Option<String>,
    rx: Option<Receiver<Result<Reply, Fail>>>,
}

impl Job {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// The reply, once it arrived. A failure goes to `error`, or to the notice bar (one message,
    /// one link) when it is a 402 or a key problem.
    fn poll(&mut self, state: &mut AiState) -> Option<Reply> {
        let rx = self.rx.take()?;
        match rx.try_recv() {
            Ok(Ok(r)) => {
                state.note_reply(&r);
                self.error = None;
                Some(r)
            }
            Ok(Err(f)) => {
                state.note_fail(&f);
                let notice = f.status == 402 || f.status == 401 || f.reprovision;
                if !notice {
                    self.error = Some(f.message);
                }
                None
            }
            Err(TryRecvError::Empty) => {
                self.rx = Some(rx);
                None
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some("The request stopped unexpectedly. Try again.".into());
                None
            }
        }
    }

    fn start(&mut self, rx: Receiver<Result<Reply, Fail>>) {
        self.result = None;
        self.error = None;
        self.rx = Some(rx);
    }
}

/// The document, story and revision a [`Fit`] was measured for.
type FitKey = (u64, StoryId, u64);

/// How much text a frame (its whole story thread) holds in its current type style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fit {
    chars: usize,
    /// The frame holds more than [`FIT_CAP`] characters.
    more: bool,
}

impl Fit {
    fn words(self) -> usize {
        (self.chars / CHARS_PER_WORD).max(1)
    }
}

/// Where a generated image goes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ImageTarget {
    doc: u64,
    /// The selected empty or graphic frame, or `None` for a new frame.
    frame: Option<ItemId>,
    /// The spread in view and the top-left corner of its page's margins, for a new frame.
    at: Option<(SpreadRef, f64, f64)>,
}

/// The image's proportions (the sizes the image line accepts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Square,
    Landscape,
    Portrait,
}

impl Shape {
    const ALL: [Shape; 3] = [Shape::Square, Shape::Landscape, Shape::Portrait];

    /// The shapes a provider can draw: publik API draws 1024x1024 only; a person's own key has all three.
    fn offered(publik: bool) -> &'static [Shape] {
        if publik { &Self::ALL[..1] } else { &Self::ALL[..] }
    }

    fn size(self) -> &'static str {
        match self {
            Shape::Square => "1024x1024",
            Shape::Landscape => "1536x1024",
            Shape::Portrait => "1024x1536",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Shape::Square => "Square",
            Shape::Landscape => "Landscape",
            Shape::Portrait => "Portrait",
        }
    }

    /// The shape nearest a frame's proportions.
    fn for_frame(width: f64, height: f64) -> Shape {
        if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
            return Shape::Square;
        }
        let ratio = width / height;
        if ratio > 1.2 {
            Shape::Landscape
        } else if ratio < 1.0 / 1.2 {
            Shape::Portrait
        } else {
            Shape::Square
        }
    }
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            setup_due: false,
            inline_setup: false,
            copy: Job::default(),
            copy_target: None,
            fit: None,
            image: Job::default(),
            image_target: None,
            shape: Shape::Square,
            unplaced: None,
        }
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// Once per frame, before drawing: finish background work, place an image that arrived, and open
/// the first-launch dialog when it is due.
pub fn frame(app: &mut DesignApp) {
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    if let Some(r) = ai.copy.poll(&mut ai.state) {
        ai.copy.result = Some(clean_copy(&r.text));
    }
    let image = ai.image.poll(&mut ai.state).map(|r| (r.bytes, ai.image_target.take()));
    ai.frames = ai.frames.saturating_add(1);
    // The first launch asks once the window is up (a few frames in). "Later" ends that for good.
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        ai.setup_due = ai.state.wants_first_run() || ai.state.owes_first_card();
    }
    let open_setup = ai.setup_due && app.ui.dialog.is_none();
    if open_setup {
        ai.setup_due = false;
        ai.state.begin();
    }
    if let Some((bytes, target)) = image {
        place_generated(app, bytes, target);
    }
    if open_setup && app.ui.dialog.is_none() {
        app.ui.dialog = Some(Dialog::new(AI_SETUP, json!({})));
    }
}

/// Object › AI › Draft Copy… and Generate Image…: open the tool's dialog. Nothing is sent here.
pub fn open(app: &mut DesignApp, dialog: &str) -> Result<Value, String> {
    if dialog == AI_GENERATE_IMAGE {
        let shape = image_target(app).and_then(|t| frame_size(app, t.frame?)).map(|(w, h)| Shape::for_frame(w, h));
        if let Some(ai) = app.ai.as_mut()
            && !ai.image.busy()
        {
            ai.shape = shape.unwrap_or(Shape::Square);
            ai.image.result = None;
            ai.image.error = None;
        }
    }
    app.ui.dialog = Some(Dialog::new(dialog, json!({})));
    Ok(Value::Null)
}

/// One of the AI dialogs' bodies. `true` closes the dialog.
pub fn dialog(app: &mut DesignApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    match d.id.as_str() {
        AI_DRAFT_COPY => draft_copy(app, ui, d),
        AI_GENERATE_IMAGE => generate_image(app, ui, d),
        _ => setup(app, ui),
    }
}

/// An AI dialog closed with Close or Escape. Dismissing the first-launch dialog counts as Later.
pub fn dismissed(app: &mut DesignApp, id: &str) {
    if id != AI_SETUP {
        return;
    }
    let Some(ai) = app.ai.as_mut() else { return };
    if !ai.state.settings.setup_done && !ai.state.busy() {
        ai.state.settings.setup_done = true;
        let path = ai.state.app.settings_path();
        if let Err(e) = ai.state.settings.save(path.as_deref()) {
            app.status(format!("Could not save AI settings: {e}"));
        }
    }
}

fn off(ui: &mut Ui) {
    ui.label(OFF);
}

fn open_url(app: &mut DesignApp, action: Action) -> bool {
    match action {
        Action::None => false,
        Action::OpenUrl(url) => {
            app.ui.pending_urls.push(url);
            false
        }
        Action::Close => true,
    }
}

/// A tool dialog's body while AI is off (no preferences) or not set up. Set up AI… turns the
/// dialog into the setup flow. Never closes the dialog.
fn not_ready(app: &mut DesignApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    let Some(ai) = app.ai.as_mut() else {
        off(ui);
        return false;
    };
    ui.label("This tool needs AI. Set it up first: publik API (no key to paste) or your own key.");
    ui.add_space(6.0);
    if ui.button("Set up AI…").clicked() {
        ai.state.begin();
        *d = Dialog::new(AI_SETUP, json!({}));
    }
    false
}

fn error_line(ui: &mut Ui, error: Option<&str>) {
    if let Some(e) = error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
}

// ---- the setup flow (first launch) ---------------------------------------------------------------

fn setup(app: &mut DesignApp, ui: &mut Ui) -> bool {
    let Some(ai) = app.ai.as_mut() else {
        off(ui);
        return false;
    };
    let action = ai.state.setup_ui(ui);
    open_url(app, action)
}

// ---- Draft Copy ------------------------------------------------------------------------------------

/// The story that Draft Copy writes for: the one selected text frame's, or the text cursor's
/// (not text in a table cell). `(document uid, story)`.
fn copy_story(app: &DesignApp) -> Option<(u64, StoryId)> {
    let st = app.session.active()?;
    if let [id] = st.selection.items.as_slice() {
        return Some((st.uid, st.doc.item(*id)?.text_frame()?.story));
    }
    let t = st.selection.text.filter(|t| t.cell.is_none())?;
    st.doc.story(t.story)?;
    Some((st.uid, t.story))
}

/// How much text story `story` holds in its frames, set in its first paragraph's style. Measured
/// on a copy of the document, as Type › Fill with Placeholder Text does.
fn measure_fit(doc: &Document, story: StoryId) -> Option<Fit> {
    let mut sample = String::with_capacity(FIT_CAP + FIT_SAMPLE.len());
    while sample.len() < FIT_CAP {
        sample.push_str(FIT_SAMPLE);
    }
    let mut copy = doc.clone();
    copy.set_story_text(story, &sample).ok()?;
    let composed = designcraft_compose::compose_story(&copy, story, &designcraft_compose::ComposeOptions::default());
    Some(match composed.overset_at {
        Some(at) => Fit { chars: at.min(sample.len()), more: false },
        None => Fit { chars: FIT_CAP, more: true },
    })
}

/// [`measure_fit`] for the active document, kept for its revision.
fn fit_of(app: &mut DesignApp, uid: u64, story: StoryId) -> Option<Fit> {
    let st = app.session.active().filter(|st| st.uid == uid)?;
    let key = (uid, story, st.revision);
    if let Some((k, fit)) = app.ai.as_ref().and_then(|ai| ai.fit)
        && k == key
    {
        return Some(fit);
    }
    let fit = measure_fit(&st.doc, story)?;
    if let Some(ai) = app.ai.as_mut() {
        ai.fit = Some((key, fit));
    }
    Some(fit)
}

/// The copy from a chat answer: no code fence, no quotation marks around it, and one line per
/// paragraph (a blank line between paragraphs would be an empty paragraph).
fn clean_copy(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim_start().starts_with("```")).filter(|l| !l.trim().is_empty()).collect();
    let joined = lines.join("\n");
    let trimmed = joined.trim();
    for (open, close) in [('"', '"'), ('\u{201C}', '\u{201D}'), ('\u{00AB}', '\u{00BB}')] {
        if let Some(inner) = trimmed.strip_prefix(open).and_then(|t| t.strip_suffix(close))
            && !inner.contains(open)
            && !inner.contains(close)
        {
            return inner.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn copy_request(brief: &str, fit: Option<Fit>) -> String {
    let limit = match fit {
        Some(f) if f.more => format!("The frame holds more than {} characters. Keep to the length the brief asks for.", f.chars),
        Some(f) => format!("Length limit: about {} words ({} characters), so that the copy fits the frame.", f.words(), f.chars),
        None => "No frame is chosen. Keep to the length the brief asks for, or about 60 words.".to_string(),
    };
    format!("Brief: {}\n{limit}", brief.trim())
}

fn draft_copy(app: &mut DesignApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    if app.ai.as_ref().is_none_or(|ai| !ai.state.provider().is_set_up()) {
        return not_ready(app, ui, d);
    }
    let target = copy_story(app);
    let fit = target.and_then(|(uid, story)| fit_of(app, uid, story));
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else { return false };
    let provider = ai.state.provider();
    match (target, fit) {
        (Some(_), Some(f)) if f.more => ui.label(format!("For the selected text frame. It holds more than {} characters.", f.chars)),
        (Some(_), Some(f)) => ui.label(format!(
            "For the selected text frame. It holds about {} words ({} characters) in its current type style.",
            f.words(),
            f.chars
        )),
        (Some(_), None) => ui.label("For the selected text frame."),
        (None, _) => ui.label("No text frame is selected. You can write copy and copy it. To fill a frame, close this window, select a text frame, and open Draft Copy again."),
    };
    ui.add_space(6.0);
    ui.label("Brief:");
    ui.add_enabled(
        !ai.copy.busy(),
        egui::TextEdit::multiline(&mut ai.copy.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("A headline and two lines for a spring sale. Friendly tone."),
    );
    ui.add_space(6.0);
    let mut start = false;
    ui.horizontal(|ui| {
        if ai.copy.busy() {
            ui.spinner();
            ui.label("Writing…");
        } else if ui.add_enabled(!ai.copy.prompt.trim().is_empty(), egui::Button::new("Write copy")).clicked() {
            start = true;
        }
    });
    error_line(ui, ai.copy.error.as_deref());
    let mut fill = None;
    if let Some(copy) = ai.copy.result.clone() {
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("ai_copy_result").max_height(200.0).show(ui, |ui| {
            ui.add(egui::Label::new(&copy).selectable(true));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(story) = ai.copy_target
                && ui.button("Fill frame").clicked()
            {
                fill = Some((story, copy.clone()));
            }
            if ui.button("Copy").clicked() {
                ctx.copy_text(copy.clone());
            }
        });
    }
    ui.add_space(4.0);
    let action = ai.state.result_bar(ui);
    if start {
        ai.copy_target = target;
        let user = copy_request(&ai.copy.prompt, fit);
        ai.copy.start(spawn(&ctx, move || client::chat(&provider, COPY_SYSTEM, &user)));
    }
    let mut close = open_url(app, action);
    if let Some(((uid, story), copy)) = fill {
        match fill_frame(app, uid, story, &copy) {
            Ok(note) => {
                app.status(note);
                close = true;
            }
            Err(e) => {
                if let Some(ai) = app.ai.as_mut() {
                    ai.copy.error = Some(e);
                }
            }
        }
    }
    close
}

/// Fill frame: the story's whole text becomes `copy` (one undo step). The note says whether it fits.
fn fill_frame(app: &mut DesignApp, uid: u64, story: StoryId, copy: &str) -> Result<String, String> {
    let alive = app.session.active().is_some_and(|st| st.uid == uid && st.doc.story(story).is_some());
    if !alive {
        return Err("The text frame is not in the active document any more. Select a text frame and try again.".into());
    }
    app.run("story.setText", json!({"story": story.0, "text": copy}))?;
    let overset = app.run("story.get", json!({"story": story.0})).ok().is_some_and(|v| !v["overset"].is_null());
    Ok(if overset {
        "Filled the frame. Some of the copy does not fit: it is overset.".into()
    } else {
        "Filled the frame with the drafted copy.".into()
    })
}

// ---- Generate Image --------------------------------------------------------------------------------

/// Where an image generated now would go: the one selected empty or graphic frame (the frames
/// File › Place fills), else a new frame on the page in view.
fn image_target(app: &DesignApp) -> Option<ImageTarget> {
    let st = app.session.active()?;
    let frame = match st.selection.items.as_slice() {
        [id] => st.doc.item(*id).filter(|it| matches!(it.content, Content::Unassigned | Content::Graphic(_))).map(|_| *id),
        _ => None,
    };
    let at = crate::canvas::current_spread_page(app).and_then(|(spread, page)| {
        let r = st.doc.spread(spread)?.pages.get(page)?.margin_rect();
        Some((spread, r.x0, r.y0))
    });
    Some(ImageTarget { doc: st.uid, frame, at })
}

fn frame_size(app: &DesignApp, frame: ItemId) -> Option<(f64, f64)> {
    let b = app.session.active()?.doc.item(frame)?.bounds();
    Some((b.width(), b.height()))
}

/// The `file.place` parameters for PNG `bytes` at `target` in the active document.
fn place_params(bytes: &[u8], target: &ImageTarget, frame_alive: bool) -> Value {
    let mut p = json!({"base64": client::base64(bytes), "name": "generated-image.png"});
    if let Some(frame) = target.frame.filter(|_| frame_alive) {
        p["frame"] = json!(frame.0);
    } else if let Some((spread, x, y)) = target.at {
        p["spread"] = json!(spread);
        p["x"] = json!(x);
        p["y"] = json!(y);
    } else {
        // A new frame (not the selection's) on the first spread, where File › Place puts it.
        p["spread"] = json!(0);
    }
    p
}

/// Place a generated image: in its target document (made active again when another one is), else
/// on the page in view of the active document. One undo step.
fn place(app: &mut DesignApp, bytes: &[u8], target: Option<ImageTarget>) -> Result<(), String> {
    if app.session.active().is_none() {
        return Err("Open a document, then click Place image.".into());
    }
    let index = target.and_then(|t| app.session.documents().iter().position(|d| d.uid == t.doc));
    if let Some(i) = index
        && app.session.active().is_some_and(|st| target.is_some_and(|t| t.doc != st.uid))
    {
        app.run("file.activate", json!({"index": i}))?;
    }
    let target = match target.filter(|_| index.is_some()) {
        Some(t) => t,
        None => image_target(app).map(|t| ImageTarget { frame: None, ..t }).ok_or("Open a document, then click Place image.")?,
    };
    let alive = target.frame.is_some_and(|f| app.session.active().is_some_and(|st| st.doc.item(f).is_some()));
    app.run("file.place", place_params(bytes, &target, alive)).map(|_| ())
}

fn place_generated(app: &mut DesignApp, bytes: Vec<u8>, target: Option<ImageTarget>) {
    match place(app, &bytes, target) {
        Ok(()) => {
            app.status("Placed the generated image.");
            if let Some(ai) = app.ai.as_mut() {
                ai.unplaced = None;
                ai.image.result = Some("The image is on the page. Edit › Undo removes it.".into());
            }
        }
        Err(e) => {
            if let Some(ai) = app.ai.as_mut() {
                ai.unplaced = Some(bytes);
                ai.image.error = Some(format!("The image is ready, but it could not be placed: {e}"));
            }
        }
    }
}

/// The cost sentence shown before an image request: the crate's (from publik, or its own), or
/// who bills an own key.
fn cost_line(state: &AiState, provider: &Provider) -> String {
    match provider {
        Provider::Publik { .. } => match &state.credential {
            Some(c) if !c.cost_sentence.trim().is_empty() => c.cost_sentence.clone(),
            _ => WHY_IT_COSTS.to_string(),
        },
        Provider::Own { service, .. } => format!("Your own key's service ({}) bills this request, not publik.", service.name()),
        Provider::NotSetUp => String::new(),
    }
}

fn generate_image(app: &mut DesignApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    if app.ai.as_ref().is_none_or(|ai| !ai.state.provider().is_set_up()) {
        return not_ready(app, ui, d);
    }
    let target = image_target(app);
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else { return false };
    let provider = ai.state.provider();
    let images = provider.supports(Line::Images);
    match target {
        Some(t) if t.frame.is_some() => ui.label("The image goes into the selected frame."),
        Some(_) => ui.label("The image goes into a new frame on the page in view."),
        None => ui.label("Open or create a document first. The image goes on its page."),
    };
    ui.add_space(6.0);
    ui.label("Describe the image:");
    ui.add_enabled(
        !ai.image.busy(),
        egui::TextEdit::multiline(&mut ai.image.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("A watercolor lighthouse at dawn, soft light, no text"),
    );
    ui.add_space(4.0);
    let shapes = Shape::offered(provider.is_publik());
    if !shapes.contains(&ai.shape) {
        ai.shape = Shape::Square;
    }
    ui.horizontal(|ui| {
        ui.label("Shape:");
        for s in shapes.iter().copied() {
            ui.add_enabled_ui(!ai.image.busy(), |ui| ui.radio_value(&mut ai.shape, s, s.label()));
        }
    });
    ui.add_space(6.0);
    if images {
        let cost = cost_line(&ai.state, &provider);
        if !cost.is_empty() {
            ui.label(RichText::new(cost).weak());
        }
    } else {
        ui.colored_label(ui.visuals().warn_fg_color, Fail::unsupported(Line::Images).message);
    }
    ui.add_space(6.0);
    let mut start = false;
    let mut place_again = false;
    ui.horizontal(|ui| {
        if ai.image.busy() {
            ui.spinner();
            ui.label("Generating… This can take a minute.");
        } else {
            let can = images && target.is_some() && !ai.image.prompt.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Generate image")).clicked() {
                start = true;
            }
            if ai.unplaced.is_some() && ui.add_enabled(target.is_some(), egui::Button::new("Place image")).clicked() {
                place_again = true;
            }
        }
    });
    error_line(ui, ai.image.error.as_deref());
    if let Some(note) = &ai.image.result {
        ui.add_space(4.0);
        ui.label(note);
    }
    ui.add_space(4.0);
    let action = ai.state.result_bar(ui);
    if start {
        ai.image_target = target;
        ai.unplaced = None;
        let prompt = ai.image.prompt.trim().to_string();
        let size = if provider.is_publik() { Shape::Square.size() } else { ai.shape.size() };
        ai.image.start(spawn(&ctx, move || client::image_generate(&provider, &prompt, size)));
    }
    if place_again && let Some(bytes) = ai.unplaced.take() {
        ai.image.error = None;
        // The page in view now: the first target is what failed.
        place_generated(app, bytes, None);
    }
    open_url(app, action)
}

// ---- Preferences › AI ----------------------------------------------------------------------------

/// The AI section of Edit › Preferences: status, sign-in and key controls, and the setup flow in
/// place. AI settings take effect at once; the dialog's OK and Cancel don't change them.
pub fn options_section(app: &mut DesignApp, ui: &mut Ui) {
    let Some(ai) = app.ai.as_mut() else {
        ui.label(RichText::new(OFF).weak());
        return;
    };
    ui.label(RichText::new("Object › AI › Draft Copy and Generate Image use publik API or your own key. Changes here take effect at once.").weak());
    ui.add_space(6.0);
    let action = if ai.inline_setup {
        let action = ai.state.setup_ui(ui);
        if action == Action::Close {
            ai.inline_setup = false;
        }
        action
    } else {
        let mut open_setup = false;
        let action = ai.state.options_ui(ui, &mut open_setup);
        ai.inline_setup = open_setup;
        action
    };
    open_url(app, action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_engine::Session;

    /// A 1×1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn app() -> DesignApp {
        let mut app = DesignApp::new(Session::new(), crate::Services::default());
        app.run("file.new", json!({"pages": 2})).unwrap();
        app
    }

    #[test]
    fn copy_comes_out_clean() {
        assert_eq!(clean_copy("```\nSpring sale\n\nTen percent off.\n```"), "Spring sale\nTen percent off.");
        assert_eq!(clean_copy("\u{201C}Fresh bread daily.\u{201D}"), "Fresh bread daily.");
        assert_eq!(clean_copy("\"One\" and \"two\""), "\"One\" and \"two\"");
        assert_eq!(clean_copy("a\r\n\r\nb  \n"), "a\nb");
        assert_eq!(clean_copy(""), "");
    }

    #[test]
    fn shapes_follow_the_frame() {
        assert_eq!(Shape::for_frame(300.0, 100.0), Shape::Landscape);
        assert_eq!(Shape::for_frame(100.0, 300.0), Shape::Portrait);
        assert_eq!(Shape::for_frame(100.0, 110.0), Shape::Square);
        assert_eq!(Shape::for_frame(0.0, 100.0), Shape::Square);
        assert_eq!(Shape::for_frame(f64::NAN, 100.0), Shape::Square);
        assert_eq!(Shape::Landscape.size(), "1536x1024");
    }

    #[test]
    fn the_request_names_the_length_limit() {
        assert!(copy_request(" sale ", Some(Fit { chars: 600, more: false })).contains("about 100 words (600 characters)"));
        assert!(copy_request("sale", Some(Fit { chars: FIT_CAP, more: true })).contains("more than"));
        assert!(copy_request("sale", None).starts_with("Brief: sale\n"));
    }

    #[test]
    fn draft_copy_measures_the_selected_frame_and_fills_it() {
        let mut app = app();
        let small = app.run("frame.create", json!({"rect": [36, 36, 200, 80], "content": "text", "text": "Old"})).unwrap()["id"].as_u64().unwrap();
        app.run("selection.set", json!({"ids": [small]})).unwrap();
        let (uid, story) = copy_story(&app).unwrap();
        let fit = measure_fit(&app.session.active().unwrap().doc, story).unwrap();
        assert!(!fit.more && fit.chars > 0 && fit.chars < FIT_CAP, "{fit:?}");
        // A bigger frame holds more.
        let big = app.run("frame.create", json!({"rect": [36, 100, 400, 500], "content": "text", "text": "Old"})).unwrap()["id"].as_u64().unwrap();
        app.run("selection.set", json!({"ids": [big]})).unwrap();
        let (_, big_story) = copy_story(&app).unwrap();
        assert!(measure_fit(&app.session.active().unwrap().doc, big_story).unwrap().chars > fit.chars);
        // The measure leaves the document alone; Fill frame is one undoable command.
        assert_eq!(app.run("story.get", json!({"story": story.0})).unwrap()["text"], "Old");
        let note = fill_frame(&mut app, uid, story, "New copy").unwrap();
        assert!(note.starts_with("Filled the frame"), "{note}");
        assert_eq!(app.run("story.get", json!({"story": story.0})).unwrap()["text"], "New copy");
        let long = "Too long. ".repeat(200);
        assert!(fill_frame(&mut app, uid, story, &long).unwrap().contains("overset"));
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(app.run("story.get", json!({"story": story.0})).unwrap()["text"], "New copy");
        assert!(fill_frame(&mut app, uid ^ 1, story, "x").is_err(), "another document");
    }

    #[test]
    fn draft_copy_needs_one_text_frame() {
        let mut app = app();
        let graphic = app.run("frame.create", json!({"rect": [36, 36, 200, 200], "content": "graphic"})).unwrap()["id"].as_u64().unwrap();
        app.run("selection.set", json!({"ids": [graphic]})).unwrap();
        assert_eq!(copy_story(&app), None);
        app.run("selection.set", json!({"ids": []})).unwrap();
        assert_eq!(copy_story(&app), None);
    }

    #[test]
    fn a_generated_image_fills_the_selected_frame_or_a_new_one() {
        let png = client::unbase64(PNG).unwrap();
        let mut app = app();
        // Nothing selected: a new frame.
        app.run("selection.set", json!({"ids": []})).unwrap();
        let target = image_target(&app).unwrap();
        assert_eq!(target.frame, None);
        let before = app.session.active().unwrap().doc.spreads[0].items.len();
        place(&mut app, &png, Some(target)).unwrap();
        assert_eq!(app.session.active().unwrap().doc.spreads[0].items.len(), before + 1);
        // An empty frame selected: the image goes into it.
        let empty = app.run("frame.create", json!({"rect": [36, 300, 236, 400], "content": "unassigned"})).unwrap()["id"].as_u64().unwrap();
        app.run("selection.set", json!({"ids": [empty]})).unwrap();
        let target = image_target(&app).unwrap();
        assert_eq!(target.frame, Some(ItemId(empty)));
        assert_eq!(frame_size(&app, ItemId(empty)).map(|(w, h)| Shape::for_frame(w, h)), Some(Shape::Landscape));
        place(&mut app, &png, Some(target)).unwrap();
        assert!(app.session.active().unwrap().doc.item(ItemId(empty)).unwrap().graphic().is_some());
        // A text frame keeps its text: the image goes into a new frame.
        let text = app.run("frame.create", json!({"rect": [36, 36, 200, 200], "content": "text", "text": "Keep"})).unwrap()["id"].as_u64().unwrap();
        app.run("selection.set", json!({"ids": [text]})).unwrap();
        assert_eq!(image_target(&app).unwrap().frame, None);
    }

    #[test]
    fn an_image_without_a_document_waits() {
        let png = client::unbase64(PNG).unwrap();
        let mut app = DesignApp::new(Session::new(), crate::Services::default());
        assert!(place(&mut app, &png, None).is_err());
        assert!(image_target(&app).is_none());
    }

    #[test]
    fn the_ai_dialogs_open_from_the_object_menu_and_send_nothing_on_their_own() {
        let mut app = app();
        // No AI in test sessions (no settings directory): the dialogs open and explain that.
        for (cmd, id) in [("app.aiDraftCopy", AI_DRAFT_COPY), ("app.aiGenerateImage", AI_GENERATE_IMAGE)] {
            app.run(cmd, json!({})).unwrap();
            assert_eq!(app.ui.dialog.as_ref().map(|d| d.id.as_str()), Some(id));
            // OK from the control channel closes it: only the dialog's own button sends.
            assert_eq!(crate::dialogs::confirm(&mut app).unwrap(), Value::Null);
            assert!(app.ui.dialog.is_none());
        }
        dismissed(&mut app, AI_SETUP);
        frame(&mut app);
        assert!(app.ui.dialog.is_none());
    }
}

#[cfg(test)]
mod publik_shape_tests {
    use super::Shape;

    #[test]
    fn publik_api_offers_square_only() {
        assert_eq!(Shape::offered(true).len(), 1);
        assert_eq!(Shape::offered(true)[0].size(), "1024x1024");
        assert_eq!(Shape::offered(false).len(), 3);
    }
}
