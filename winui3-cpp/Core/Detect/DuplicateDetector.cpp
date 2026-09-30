#include "DuplicateDetector.h"

namespace EchoDup::Core
{
std::vector<DetectionResult> DuplicateDetector::Scan(
    const std::vector<AudioFile>& files)
{
    std::vector<DetectionResult> results;

    for(size_t i=0;i<files.size();++i)
    {
        for(size_t j=i+1;j<files.size();++j)
        {
            DetectionResult result;
            result.firstPath = files[i].path;
            result.secondPath = files[j].path;
            result.similarity = 0.0;
            results.push_back(result);
        }
    }

    return results;
}
}
