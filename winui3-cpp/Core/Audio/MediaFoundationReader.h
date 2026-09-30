#pragma once
#include "AudioReader.h"

namespace EchoDup::Core
{
class MediaFoundationReader : public AudioReader
{
public:
    bool Open(const std::wstring& path) override;

    std::vector<float> ReadSamples() override;
};
}
