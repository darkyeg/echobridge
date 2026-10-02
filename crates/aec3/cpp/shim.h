// SPDX-License-Identifier: Apache-2.0
// A C interface to WebRTC AEC3, noise suppression, and the high-pass filter, for Rust.
// No C++ exception crosses it: failures return a status and a message.
#ifndef ECHOBRIDGE_AEC3_SHIM_H_
#define ECHOBRIDGE_AEC3_SHIM_H_

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct EbAec EbAec;

// Dominant near-end (user speaking) detection.
typedef struct {
    float enr_threshold;
    float snr_threshold;
    int trigger_blocks;
    int hold_blocks;
} EbNearendDetection;

// Echo-to-near-end ratios between which a suppressor mask moves from transparent to full.
typedef struct {
    float enr_transparent;
    float enr_suppress;
} EbMask;

typedef struct {
    int sample_rate;      // 16000, 32000, or 48000
    int channels;         // 1 or 2, for both microphone and reference
    int stream_delay_ms;  // 0-250; 0 lets AEC3 find the delay
    // 0 runs only the high-pass filter and noise suppression, for callers that remove the
    // leak themselves; AEC3 would otherwise suppress speech it cannot match to echo.
    int echo_cancellation;
    // AEC3 switches itself off after six seconds without a converged filter. Convergence
    // is only recognized above about -56 dBFS, so a quiet but steady headset leak would
    // turn echo removal off; 0 keeps it on.
    int allow_transparent_mode;
    int noise_suppression;  // -1 off, or 0-3 for 6, 12, 18, or 21 dB
    // Overrides; null keeps AEC3's defaults.
    const EbNearendDetection *nearend_detection;
    const EbMask *nearend_lf;
    const EbMask *nearend_hf;
    // Non-zero applies the near-end masks while the user is silent too.
    int normal_follows_nearend;
} EbAecConfig;

typedef struct {
    double echo_return_loss_db;
    double echo_return_loss_enhancement_db;
    int delay_ms;
} EbAecMetrics;

// Returns null and writes a message to `error` (if not null) on failure.
EbAec *eb_aec_create(const EbAecConfig *config, char *error, size_t error_size);
// Process one 10 ms frame: interleaved `near` and `far`, `rate / 100 * channels` floats
// each, in [-1, 1]. Returns 0, or -1 for non-finite input.
int eb_aec_process(EbAec *aec, const float *near, const float *far, float *out);
// Forget the echo path and noise estimates.
void eb_aec_reset(EbAec *aec);
void eb_aec_metrics(const EbAec *aec, EbAecMetrics *metrics);
void eb_aec_free(EbAec *aec);

#ifdef __cplusplus
}
#endif

#endif
