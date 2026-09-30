#pragma once

#include <cstddef>

namespace EchoDup::Core
{
struct ScanProgress
{
    size_t completed{0};
    size_t total{0};
    double percent{0.0};
};
}
