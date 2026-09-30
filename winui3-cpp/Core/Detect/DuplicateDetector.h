#pragma once

#include "DetectionResult.h"
#include "../Model/AudioFile.h"
#include <vector>

namespace EchoDup::Core
{
class DuplicateDetector
{
public:
    std::vector<DetectionResult> Scan(
        const std::vector<AudioFile>& files);
};
}
