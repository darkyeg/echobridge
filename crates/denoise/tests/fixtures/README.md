# Synthetic speech fixture

`synthetic-speech.wav` was generated with Windows speech synthesis for EchoBridge testing. It contains no microphone recording or user voice.

The quiet speech regression mixes this spoken waveform with a separate stereo playback signal and a known delayed echo path. It measures correlation and gain of the known speech component, rather than treating total output volume as evidence that speech survived. Passing this fixture is not acceptance of the user's actual headset or call.
