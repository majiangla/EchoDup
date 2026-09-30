#pragma once
#include <vector>
#include "../Model/AudioFile.h"

namespace EchoDup::Core {

struct MatchResult
{
    AudioFile first;
    AudioFile second;
    double similarity{};
};

class DuplicateDetector
{
public:
    std::vector<MatchResult> Scan(
        const std::vector<AudioFile>& files);
};

}
