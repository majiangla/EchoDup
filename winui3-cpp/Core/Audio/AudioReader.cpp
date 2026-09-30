#include "AudioReader.h"

namespace EchoDup::Core
{
bool AudioReader::Open(const wchar_t*)
{
    return true;
}

std::vector<float> AudioReader::ReadSamples()
{
    return {};
}
}
