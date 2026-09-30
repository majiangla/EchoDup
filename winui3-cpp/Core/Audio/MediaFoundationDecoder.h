#pragma once

#include "AudioBuffer.h"
#include <string>

namespace EchoDup::Audio
{
class MediaFoundationDecoder
{
public:
    bool Open(const std::wstring& path);
    AudioBuffer Decode();
};
}
