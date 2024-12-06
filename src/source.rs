use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::utils::hashbrown::HashMap;
use bevy::{audio::Source, prelude::*, utils::Duration};
use helgoboss_midi::StructuredShortMessage;
use rustysynth::SoundFont;

use crate::midi::{MidiEvent, MidiMetaEvent, MidiTrack};

#[derive(Asset, TypePath)]
pub struct MidiAudio {
	midi_track: MidiTrack,
	soundfont: Arc<SoundFont>,
	voices: Vec<Voice>,
	channels: Vec<Channel>,
	num_audio_channels: u16,
	current_audio_channel: u16,
	samples_per_second: f64,
	ticks_per_sample: f64,
	beats_per_second: f64,
	tick: f64,
	event_index: usize,
	preset_index: HashMap<(u8, u8), usize>,
	buffer: Arc<Mutex<VecDeque<i16>>>,
	buffer_events: Vec<(Instant, MidiBufferMessage)>,
	buffer_event_now: Instant,
}

impl MidiAudio {
	pub fn new(midi_track: MidiTrack, soundfont: Arc<SoundFont>) -> Self {
		let mut buffer_events = Vec::new();

		let samples_per_second = 44100.0;
		let beats_per_second = 120.0 / 60.0;
		let ticks_per_beat = midi_track.ticks_per_beat as f64;
		let ticks_per_sample = (ticks_per_beat * beats_per_second) / samples_per_second;
		buffer_events.push((
			Instant::now(),
			MidiBufferMessage::TempoChange { beats_per_second },
		));
		let channels = (0..16)
			.map(|i| Channel {
				bank_number: if i == 9 { 128 } else { 0 },
				patch_number: 0,
			})
			.collect();

		let preset_index = soundfont
			.get_presets()
			.iter()
			.enumerate()
			.map(|(index, preset)| {
				(
					(
						preset.get_bank_number() as u8,
						preset.get_patch_number() as u8,
					),
					index,
				)
			})
			.collect();

		Self {
			midi_track,
			soundfont,
			voices: vec![],
			channels,
			num_audio_channels: 2,
			current_audio_channel: 0,
			samples_per_second,
			ticks_per_sample,
			beats_per_second,
			tick: 0.0,
			event_index: 0,
			preset_index,
			buffer: Arc::new(Mutex::new(VecDeque::new())),
			buffer_events,
			buffer_event_now: Instant::now(),
		}
	}

	pub fn tick(&mut self, delta: Duration) {
		self.buffer_event_now += delta;

		let ticks = delta.as_secs_f64() * self.samples_per_second;
		let max_ticks = self.samples_per_second
			- self.buffer.lock().unwrap().len() as f64 / self.num_audio_channels as f64;
		let ticks = ticks.min(max_ticks) as usize;

		let mut buffer = VecDeque::with_capacity(ticks);
		for _ in 0..ticks * self.num_audio_channels as usize {
			self.tick_once(&mut buffer);
		}

		let buffer = buffer
			.into_iter()
			.enumerate()
			.filter_map(|(i, message)| match message {
				MidiBufferMessage::Audio(sample) => Some(sample),
				_ => {
					self.buffer_events.push((
						self.buffer_event_now
							+ Duration::from_secs_f64(i as f64 / self.samples_per_second),
						message,
					));
					None
				}
			});
		self.buffer.lock().unwrap().extend(buffer);
	}

	fn tick_once(&mut self, buffer: &mut VecDeque<MidiBufferMessage>) {
		if self.current_audio_channel == 0 {
			self.tick += self.ticks_per_sample;

			while let Some(event) = self
				.midi_track
				.events
				.get(self.event_index)
				.filter(|event| event.time <= self.tick as u64)
			{
				match event.inner {
					MidiEvent::Message(StructuredShortMessage::NoteOn {
						channel,
						key_number,
						velocity,
					}) => {
						let key_number: i32 = key_number.into();
						let channel_index: usize = channel.into();
						let channel = &self.channels[channel_index];
						let Some(&preset_index) = self
							.preset_index
							.get(&(channel.bank_number, channel.patch_number))
						else {
							continue;
						};
						let preset = &self.soundfont.get_presets()[preset_index];
						let preset_regions = preset
							.get_regions()
							.iter()
							.filter(|region| region.contains(key_number, velocity.into()));
						let instruments = preset_regions.map(|region| {
							&self.soundfont.get_instruments()[region.get_instrument_id()]
						});
						let instrument_regions = instruments.flat_map(|instrument| {
							instrument
								.get_regions()
								.iter()
								.filter(|region| region.contains(key_number, velocity.into()))
						});
						let samples = instrument_regions.map(|region| {
							&self.soundfont.get_sample_headers()[region.get_sample_id()]
						});
						self.voices.push(Voice {
							parts: samples
								.map(|sample| VoicePart {
									speed: 2_f32.powf(
										(key_number as f32 - sample.get_original_pitch() as f32)
											/ 12.0,
									),
									current_sample: sample.get_start() as f64,
									end_sample: sample.get_end() as f64,
								})
								.collect(),
						});
					}
					MidiEvent::Meta(MidiMetaEvent::Tempo {
						tempo: beats_per_minute,
					}) => {
						self.beats_per_second = beats_per_minute / 60.0;
						buffer.push_back(MidiBufferMessage::TempoChange {
							beats_per_second: self.beats_per_second,
						});
						self.ticks_per_sample = (self.midi_track.ticks_per_beat as f64
							* self.beats_per_second)
							/ self.samples_per_second;
					}
					_ => {}
				}
				self.event_index += 1;

				if self.event_index >= self.midi_track.events.len() {
					self.event_index = 0;
					self.tick = 0.0;
				}
			}
		}

		let sample = self
			.voices
			.iter()
			.map(|voice| {
				let current_sample = voice.current_sample(self.current_audio_channel);
				let wave_data = self.soundfont.get_wave_data();
				let floor = wave_data[current_sample.floor() as usize];
				let ceil = wave_data[current_sample.ceil() as usize];
				let fraction = current_sample.fract() as f32;
				ceil as f32 * fraction + floor as f32 * (1.0 - fraction)
			})
			.sum::<f32>() as i16;

		if self.current_audio_channel == 0 {
			self.voices.iter_mut().for_each(Voice::tick);
			self.voices.retain(Voice::alive);
		}
		self.current_audio_channel = (self.current_audio_channel + 1) % self.num_audio_channels;

		buffer.push_back(MidiBufferMessage::Audio(sample));
	}

	pub fn clear_old_buffer_events(&mut self) {
		self.buffer_events
			.retain(|(time, _)| *time > self.buffer_event_now);
	}

	pub fn beats_per_second(&self) -> f64 {
		self.beats_per_second
	}
}

pub struct MidiDecoder {
	buffer: Arc<Mutex<VecDeque<i16>>>,
	num_audio_channels: u16,
	samples_per_second: u32,
}

impl Iterator for MidiDecoder {
	type Item = i16;

	fn next(&mut self) -> Option<Self::Item> {
		self.buffer.lock().unwrap().pop_front().or(Some(0))
	}
}

impl Source for MidiDecoder {
	fn current_frame_len(&self) -> Option<usize> {
		if self.buffer.lock().unwrap().is_empty() {
			Some(1)
		} else {
			None
		}
	}

	fn channels(&self) -> u16 {
		self.num_audio_channels
	}

	fn sample_rate(&self) -> u32 {
		self.samples_per_second
	}

	fn total_duration(&self) -> Option<Duration> {
		None
	}
}

impl Decodable for MidiAudio {
	type DecoderItem = <MidiDecoder as Iterator>::Item;

	type Decoder = MidiDecoder;

	fn decoder(&self) -> Self::Decoder {
		MidiDecoder {
			buffer: self.buffer.clone(),
			num_audio_channels: self.num_audio_channels,
			samples_per_second: self.samples_per_second as u32,
		}
	}
}

struct Voice {
	parts: Vec<VoicePart>,
}

impl Voice {
	fn tick(&mut self) {
		self.parts.iter_mut().for_each(VoicePart::tick);
	}

	fn alive(&self) -> bool {
		self.parts.iter().any(VoicePart::alive)
	}

	fn current_sample(&self, current_audio_channel: u16) -> f64 {
		let part = &self.parts[current_audio_channel as usize % self.parts.len()];
		if part.alive() {
			part.current_sample
		} else {
			0.0
		}
	}
}

struct VoicePart {
	speed: f32,
	current_sample: f64,
	end_sample: f64,
}

impl VoicePart {
	fn tick(&mut self) {
		self.current_sample += self.speed as f64;
	}

	fn alive(&self) -> bool {
		self.current_sample < self.end_sample
	}
}

struct Channel {
	bank_number: u8,
	patch_number: u8,
}

#[derive(Default, Clone)]
pub struct SyncedMidiInfo {
	pub beat: f64,
	pub beats_per_second: f64,
}

pub enum MidiBufferMessage {
	Audio(i16),
	TempoChange { beats_per_second: f64 },
}
