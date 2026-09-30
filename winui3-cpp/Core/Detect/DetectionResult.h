#pragma once

#include <string>
#include <vector>

namespace EchoDup::Core
{
struct DetectionResult
{
    std::wstring firstPath;
    std::wstring secondPath;
    double similarity{};
    double startTime{};
    double endTime{};
};
}
