#pragma once

#include <vector>
#include "AudioSegment.h"

namespace EchoDup::Core
{
class SegmentAnalyzer
{
public:
    std::vector<AudioSegment> Analyze(
        const std::vector<float>& samples);
};
}
