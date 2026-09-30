#pragma once

#include <string>
#include <cstdint>

namespace EchoDup::Core
{
struct AudioFile
{
    std::wstring path;
    std::uint64_t size{};
    std::int64_t duration{};
};
}
