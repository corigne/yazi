use std::ops::Range;

use anyhow::{anyhow, Result};
use ratatui::layout::Rect;
use tokio::sync::oneshot::Sender;
use unicode_width::UnicodeWidthStr;

use super::{InputSnap, InputSnaps};
use crate::{core::{external, Position}, misc::CharKind};

#[derive(Default)]
pub struct Input {
	snaps: InputSnaps,

	title:    String,
	position: (u16, u16),
	callback: Option<Sender<Result<String>>>,

	pub visible: bool,
}

pub struct InputOpt {
	pub title:    String,
	pub value:    String,
	pub position: Position,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMode {
	Normal,
	#[default]
	Insert,
}

impl InputMode {
	#[inline]
	fn delta(&self) -> usize { (*self != InputMode::Insert) as usize }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputOp {
	#[default]
	None,
	Delete(bool),
	Yank,
}

impl Input {
	pub fn show(&mut self, opt: InputOpt, tx: Sender<Result<String>>) {
		self.close(false);
		self.snaps.reset(opt.value);

		self.title = opt.title;
		self.position = match opt.position {
			Position::Coords(x, y) => (x, y),
			_ => unimplemented!(),
		};

		self.callback = Some(tx);
		self.visible = true;
	}

	pub fn close(&mut self, submit: bool) -> bool {
		if let Some(cb) = self.callback.take() {
			let _ =
				cb.send(if submit { Ok(self.snap_mut().value.clone()) } else { Err(anyhow!("canceled")) });
		}

		self.visible = false;
		true
	}

	pub fn escape(&mut self) -> bool {
		let snap = self.snap_mut();
		match snap.mode {
			InputMode::Normal => {
				snap.op = InputOp::None;
				snap.start = None;
			}
			InputMode::Insert => {
				snap.mode = InputMode::Normal;
				self.move_(-1);
			}
		}
		self.snaps.tag();
		true
	}

	pub fn insert(&mut self, append: bool) -> bool {
		if !self.snap_mut().insert() {
			return false;
		}
		if append {
			self.move_(1);
		}
		true
	}

	#[inline]
	pub fn visual(&mut self) -> bool {
		self.snap_mut().visual();
		self.escape()
	}

	#[inline]
	pub fn undo(&mut self) -> bool {
		self.snaps.undo();
		self.escape()
	}

	#[inline]
	pub fn redo(&mut self) -> bool { self.snaps.redo() }

	pub fn move_(&mut self, step: isize) -> bool {
		let snap = self.snap();
		let b = self.handle_op(
			if step <= 0 {
				snap.cursor.saturating_sub(step.abs() as usize)
			} else {
				snap.count().min(snap.cursor + step as usize)
			},
			false,
		);

		let snap = self.snap_mut();
		if snap.cursor < snap.offset {
			snap.offset = snap.cursor;
		} else if snap.value.is_empty() {
			snap.offset = 0;
		} else {
			let delta = snap.mode.delta();
			let s = snap.slice(snap.offset..snap.cursor + delta);
			if s.width() >= /*TODO: hardcode*/ 50 - 2 {
				let s = s.chars().rev().collect::<String>();
				snap.offset = snap.cursor - InputSnap::find_window(&s, 0).end.saturating_sub(delta);
			}
		}

		b
	}

	#[inline]
	pub fn move_in_operating(&mut self, step: isize) -> bool {
		if self.snap_mut().op == InputOp::None { false } else { self.move_(step) }
	}

	pub fn backward(&mut self) -> bool {
		let snap = self.snap();
		if snap.cursor == 0 {
			return self.move_(0);
		}

		let idx = snap.idx(snap.cursor).unwrap_or(snap.len());
		let mut it = snap.value[..idx].chars().rev().enumerate();
		let mut prev = CharKind::new(it.next().unwrap().1);
		for (i, c) in it {
			let c = CharKind::new(c);
			if prev != CharKind::Space && prev != c {
				return self.move_(-(i as isize));
			}
			prev = c;
		}

		if prev != CharKind::Space {
			return self.move_(-(snap.len() as isize));
		}
		false
	}

	pub fn forward(&mut self, end: bool) -> bool {
		let snap = self.snap();
		if snap.value.is_empty() {
			return self.move_(0);
		}

		let mut it = snap.value.chars().skip(snap.cursor).enumerate();
		let mut prev = CharKind::new(it.next().unwrap().1);
		for (i, c) in it {
			let c = CharKind::new(c);
			let b = if end {
				prev != CharKind::Space && prev != c && i != 1
			} else {
				c != CharKind::Space && c != prev
			};
			if b && snap.op != InputOp::None {
				return self.move_(i as isize);
			} else if b {
				return self.move_(if end { i - 1 } else { i } as isize);
			}
			prev = c;
		}

		self.move_(snap.len() as isize)
	}

	pub fn type_(&mut self, c: char) -> bool {
		let snap = self.snap_mut();
		if snap.cursor < 1 {
			snap.value.insert(0, c);
		} else if snap.cursor == snap.count() {
			snap.value.push(c);
		} else {
			snap.value.insert(snap.idx(snap.cursor).unwrap(), c);
		}
		self.move_(1)
	}

	pub fn backspace(&mut self) -> bool {
		let snap = self.snap_mut();
		if snap.cursor < 1 {
			return false;
		} else if snap.cursor == snap.count() {
			snap.value.pop();
		} else {
			snap.value.remove(snap.idx(snap.cursor - 1).unwrap());
		}
		self.move_(-1)
	}

	pub fn delete(&mut self, insert: bool) -> bool {
		match self.snap().op {
			InputOp::None => {
				if self.snap().start.is_some() {
					self.snap_mut().op = InputOp::Delete(insert);
					return self.handle_op(self.snap().cursor, true).then(|| self.move_(0)).is_some();
				}

				let snap = self.snap_mut();
				snap.op = InputOp::Delete(insert);
				snap.start = Some(snap.cursor);
				false
			}
			InputOp::Delete(..) => {
				self.move_(-(self.snap().len() as isize));
				self.snap_mut().value.clear();
				self.snap_mut().mode = if insert { InputMode::Insert } else { InputMode::Normal };
				true
			}
			_ => false,
		}
	}

	pub fn yank(&mut self) -> bool {
		match self.snap().op {
			InputOp::None => {
				if self.snap().start.is_some() {
					self.snap_mut().op = InputOp::Yank;
					return self.handle_op(self.snap().cursor, true).then(|| self.move_(0)).is_some();
				}

				let snap = self.snap_mut();
				snap.op = InputOp::Yank;
				snap.start = Some(snap.cursor);
				false
			}
			InputOp::Yank => {
				self.snap_mut().start = Some(0);
				self.move_(self.snap().len() as isize);
				false
			}
			_ => false,
		}
	}

	pub fn paste(&mut self, before: bool) -> bool {
		if self.snap().start.is_some() {
			self.snap_mut().op = InputOp::Delete(false);
			self.handle_op(self.snap().cursor, true);
		}

		let str =
			futures::executor::block_on(async { external::clipboard_get().await }).unwrap_or_default();
		if str.is_empty() {
			return false;
		}

		self.insert(!before);
		for c in str.chars() {
			self.type_(c);
		}
		self.escape();
		true
	}

	fn handle_op(&mut self, cursor: usize, include: bool) -> bool {
		let old = self.snap().clone();
		let snap = self.snap_mut();
		let range = if snap.op == InputOp::None { None } else { snap.range(cursor, include) };

		match snap.op {
			InputOp::None => {
				snap.cursor = cursor;
			}
			InputOp::Delete(insert) => {
				let range = range.unwrap();
				let Range { start, end } = snap.idx(range.start)..snap.idx(range.end);

				snap.value.drain(start.unwrap()..end.unwrap());
				snap.mode = if insert { InputMode::Insert } else { InputMode::Normal };
				snap.cursor = range.start;
			}
			InputOp::Yank => {
				let range = range.unwrap();
				let Range { start, end } = snap.idx(range.start)..snap.idx(range.end);
				let yanked = &snap.value[start.unwrap()..end.unwrap()];

				futures::executor::block_on(async {
					external::clipboard_set(yanked).await.ok();
				});
			}
		};

		snap.op = InputOp::None;
		snap.cursor = snap.count().saturating_sub(snap.mode.delta()).min(snap.cursor);
		if *snap == old {
			return false;
		}

		if old.op != InputOp::None {
			self.snaps.tag();
		}
		true
	}
}

impl Input {
	#[inline]
	pub fn title(&self) -> String { self.title.clone() }

	#[inline]
	pub fn value(&self) -> &str { self.snap().slice(self.snap().window()) }

	#[inline]
	pub fn mode(&self) -> InputMode { self.snap().mode }

	#[inline]
	pub fn area(&self) -> Rect {
		// TODO: hardcode
		Rect { x: self.position.0, y: self.position.1 + 2, width: 50, height: 3 }
	}

	#[inline]
	pub fn cursor(&self) -> (u16, u16) {
		let snap = self.snap();
		let width = snap.slice(snap.offset..snap.cursor).width() as u16;

		let area = self.area();
		(area.x + width + 1, area.y + 1)
	}

	pub fn selected(&self) -> Option<Rect> {
		let snap = self.snap();
		if snap.start.is_none() {
			return None;
		}

		let start = snap.start.unwrap();
		let (start, end) =
			if start < snap.cursor { (start, snap.cursor) } else { (snap.cursor + 1, start + 1) };

		let win = snap.window();
		let Range { start, end } = start.max(win.start)..end.min(win.end);

		Some(Rect {
			x:      self.position.0 + 1 + snap.slice(snap.offset..start).width() as u16,
			y:      self.position.1 + 3,
			width:  snap.slice(start..end).width() as u16,
			height: 1,
		})
	}

	#[inline]
	fn snap(&self) -> &InputSnap { self.snaps.current() }

	#[inline]
	fn snap_mut(&mut self) -> &mut InputSnap { self.snaps.current_mut() }
}
