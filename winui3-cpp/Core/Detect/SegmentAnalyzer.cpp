#include "SegmentAnalyzer.h"

namespace EchoDup::Core
{
std::vector<AudioSegment> SegmentAnalyzer::Analyze(
    const std::vector<float>& samples)
{
    std::vector<AudioSegment> result;

    if(samples.empty())
        return result;

    AudioSegment segment;
    segment.startSeconds = 0;
    segment.endSeconds = static_cast<double>(samples.size()) / 48000.0;
    segment.sampleOffset = 0;

    result.push_back(segment);
    return result;
}
}
