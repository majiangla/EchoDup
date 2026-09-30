#pragma once

#include <vector>
#include <cstdint>

namespace EchoDup::Audio
{
struct AudioBuffer
{
    std::vector<float> samples;
    uint32_t sampleRate{48000};
    uint16_t channels{1};
    double duration{0.0};
};
}
