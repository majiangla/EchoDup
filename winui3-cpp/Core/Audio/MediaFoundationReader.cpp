#include "MediaFoundationReader.h"

namespace EchoDup::Core
{
bool MediaFoundationReader::Open(const std::wstring&)
{
    return true;
}

std::vector<float> MediaFoundationReader::ReadSamples()
{
    return {};
}
}
