#pragma once

#include <string>

namespace EchoDup::Audio
{
class PlaybackEngine
{
public:
    bool Open(const std::wstring& path);
    void Play();
    void Pause();
    void Seek(double seconds);
};
}
