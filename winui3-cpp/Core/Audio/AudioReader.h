#pragma once

#include <vector>

namespace EchoDup::Core
{
class AudioReader
{
public:
    bool Open(const wchar_t* path);
    std::vector<float> ReadSamples();
};
}
