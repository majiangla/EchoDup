#pragma once

#include <cstdint>

namespace EchoDup::Core
{
struct AudioSegment
{
    double startSeconds{};
    double endSeconds{};
    std::uint64_t sampleOffset{};
};
}
