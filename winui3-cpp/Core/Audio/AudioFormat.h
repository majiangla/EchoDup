#pragma once

#include <cstdint>

namespace EchoDup::Audio
{
struct AudioFormat
{
    uint32_t sampleRate{48000};
    uint16_t channels{1};
    uint16_t bitsPerSample{32};
};
}
