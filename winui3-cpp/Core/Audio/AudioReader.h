#pragma once

#include <vector>
#include <string>

namespace EchoDup::Core
{
class AudioReader
{
public:
    virtual ~AudioReader() = default;

    virtual bool Open(const std::wstring& path) = 0;

    virtual std::vector<float> ReadSamples() = 0;
};
}
