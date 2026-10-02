// SPDX-License-Identifier: Apache-2.0
#include "shim.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <exception>
#include <memory>
#include <stdexcept>

#include "api/echo_canceller3_config.h"
#include "api/echo_canceller3_factory.h"
#include "api/echo_control.h"
#include "audio_processing/audio_buffer.h"
#include "audio_processing/high_pass_filter.h"
#include "audio_processing/ns/noise_suppressor.h"

struct EbAec {
    EbAecConfig config;
    EbNearendDetection detection;
    EbMask lf, hf;
    int frame;
    std::unique_ptr<webrtc::EchoControl> echo;
    std::unique_ptr<webrtc::AudioBuffer> near_buffer;
    std::unique_ptr<webrtc::AudioBuffer> far_buffer;
    std::unique_ptr<webrtc::HighPassFilter> high_pass;
    std::unique_ptr<webrtc::NoiseSuppressor> noise;

    explicit EbAec(const EbAecConfig &source) : config(source), detection{}, lf{}, hf{} {
        // Own copies of the overrides, so the caller's may go away.
        if (source.nearend_detection) {
            detection = *source.nearend_detection;
            config.nearend_detection = &detection;
        }
        if (source.nearend_lf) {
            lf = *source.nearend_lf;
            config.nearend_lf = &lf;
        }
        if (source.nearend_hf) {
            hf = *source.nearend_hf;
            config.nearend_hf = &hf;
        }
        const int rate = config.sample_rate;
        if (rate != 16000 && rate != 32000 && rate != 48000)
            throw std::invalid_argument("Sample rate must be 16000, 32000, or 48000 Hz");
        if (config.channels < 1 || config.channels > 2)
            throw std::invalid_argument("Use one or two channels");
        if (config.stream_delay_ms < 0 || config.stream_delay_ms > 250)
            throw std::invalid_argument("Echo delay must be between 0 and 250 ms");
        if (config.noise_suppression < -1 || config.noise_suppression > 3)
            throw std::invalid_argument("Noise suppression is -1 (off) or 0-3");
        frame = rate / 100;
        reset();
    }

    webrtc::EchoCanceller3Config echo_config() const {
        webrtc::EchoCanceller3Config result;
        auto &suppressor = result.suppressor;
        if (config.nearend_detection) {
            auto &d = suppressor.dominant_nearend_detection;
            d.enr_threshold = detection.enr_threshold;
            d.snr_threshold = detection.snr_threshold;
            d.trigger_threshold = detection.trigger_blocks;
            d.hold_duration = detection.hold_blocks;
        }
        if (config.nearend_lf) {
            suppressor.nearend_tuning.mask_lf.enr_transparent = lf.enr_transparent;
            suppressor.nearend_tuning.mask_lf.enr_suppress = lf.enr_suppress;
        }
        if (config.nearend_hf) {
            suppressor.nearend_tuning.mask_hf.enr_transparent = hf.enr_transparent;
            suppressor.nearend_tuning.mask_hf.enr_suppress = hf.enr_suppress;
        }
        if (config.normal_follows_nearend) suppressor.normal_tuning = suppressor.nearend_tuning;
        result.ep_strength.bounded_erl = !config.allow_transparent_mode;
        if (!webrtc::EchoCanceller3Config::Validate(&result))
            throw std::invalid_argument("Invalid echo cancellation configuration");
        return result;
    }

    void reset() {
        const int rate = config.sample_rate, channels = config.channels;
        echo = webrtc::EchoCanceller3Factory(echo_config()).Create(rate, channels, channels);
        near_buffer = std::make_unique<webrtc::AudioBuffer>(rate, channels, rate, channels, rate, channels);
        far_buffer = std::make_unique<webrtc::AudioBuffer>(rate, channels, rate, channels, rate, channels);
        high_pass = std::make_unique<webrtc::HighPassFilter>(rate, channels);
        noise.reset();
        if (config.noise_suppression >= 0) {
            webrtc::NsConfig ns;
            ns.target_level = static_cast<webrtc::NsConfig::SuppressionLevel>(config.noise_suppression);
            noise = std::make_unique<webrtc::NoiseSuppressor>(ns, rate, channels);
        }
    }

    // WebRTC works in 16-bit sample units held as floats.
    bool load(const float *samples, webrtc::AudioBuffer *buffer) const {
        for (int channel = 0; channel < config.channels; ++channel) {
            float *target = buffer->channels()[channel];
            for (int index = 0; index < frame; ++index) {
                const float sample = samples[index * config.channels + channel];
                if (!std::isfinite(sample)) return false;
                target[index] = std::clamp(sample * 32767.f, -32768.f, 32767.f);
            }
        }
        return true;
    }

    int process(const float *near_samples, const float *far_samples, float *out) {
        if (!load(near_samples, near_buffer.get()) || !load(far_samples, far_buffer.get())) return -1;
        if (config.echo_cancellation) {
            far_buffer->SplitIntoFrequencyBands();
            echo->AnalyzeRender(far_buffer.get());
            echo->AnalyzeCapture(near_buffer.get());
        }
        near_buffer->SplitIntoFrequencyBands();
        high_pass->Process(near_buffer.get(), true);
        if (config.echo_cancellation) {
            echo->SetAudioBufferDelay(config.stream_delay_ms);
            echo->ProcessCapture(near_buffer.get(), nullptr, false);
        }
        if (noise) {
            noise->Analyze(*near_buffer);
            noise->Process(near_buffer.get());
        }
        near_buffer->MergeFrequencyBands();
        for (int channel = 0; channel < config.channels; ++channel) {
            const float *source = near_buffer->channels_const()[channel];
            for (int index = 0; index < frame; ++index)
                out[index * config.channels + channel] = source[index] / 32767.f;
        }
        return 0;
    }
};

namespace {
void write_error(char *error, size_t size, const char *message) {
    if (error && size) std::snprintf(error, size, "%s", message);
}
}  // namespace

extern "C" {

EbAec *eb_aec_create(const EbAecConfig *config, char *error, size_t error_size) {
    if (!config) {
        write_error(error, error_size, "No configuration");
        return nullptr;
    }
    try {
        return new EbAec(*config);
    } catch (const std::exception &exc) {
        write_error(error, error_size, exc.what());
    } catch (...) {
        write_error(error, error_size, "Unknown error");
    }
    return nullptr;
}

int eb_aec_process(EbAec *aec, const float *near_samples, const float *far_samples, float *out) {
    try {
        return aec->process(near_samples, far_samples, out);
    } catch (...) {
        return -2;
    }
}

void eb_aec_reset(EbAec *aec) {
    try {
        aec->reset();
    } catch (...) {
        // The configuration was validated at creation, so this cannot fail in practice.
    }
}

void eb_aec_metrics(const EbAec *aec, EbAecMetrics *metrics) {
    const auto values = aec->echo->GetMetrics();
    metrics->echo_return_loss_db = values.echo_return_loss;
    metrics->echo_return_loss_enhancement_db = values.echo_return_loss_enhancement;
    metrics->delay_ms = values.delay_ms;
}

void eb_aec_free(EbAec *aec) { delete aec; }
}
