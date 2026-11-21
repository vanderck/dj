// C interface to Signalsmith Stretch for dj: stereo, interleaved buffers.
#include <signalsmith-stretch.h>
#include <cstddef>

namespace {
struct Interleaved {
    float *data;
    struct Channel {
        float *data;
        int c;
        float &operator[](int i) const { return data[2 * i + c]; }
    };
    Channel operator[](int c) const { return Channel{data, c}; }
};
}

struct dj_stretch {
    signalsmith::stretch::SignalsmithStretch<float> s{12345};
};

extern "C" {

dj_stretch *dj_stretch_new(float sample_rate) {
    auto *h = new dj_stretch;
    // The default preset (120 ms blocks every 30 ms), its work spread over the calls.
    h->s.presetDefault(2, sample_rate, true);
    return h;
}

void dj_stretch_free(dj_stretch *h) { delete h; }

void dj_stretch_reset(dj_stretch *h) { h->s.reset(); }

int dj_stretch_input_latency(const dj_stretch *h) { return h->s.inputLatency(); }

int dj_stretch_output_latency(const dj_stretch *h) { return h->s.outputLatency(); }

int dj_stretch_block(const dj_stretch *h) { return h->s.blockSamples() + h->s.intervalSamples(); }

void dj_stretch_transpose(dj_stretch *h, float factor) { h->s.setTransposeFactor(factor); }

void dj_stretch_seek(dj_stretch *h, float *input, int frames, double rate) {
    h->s.seek(Interleaved{input}, frames, rate);
}

void dj_stretch_process(dj_stretch *h, float *input, int in_frames, float *output, int out_frames) {
    h->s.process(Interleaved{input}, in_frames, Interleaved{output}, out_frames);
}

}
