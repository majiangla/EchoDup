#pragma once

#include <vector>

namespace EchoDup::Core
{
struct Fingerprint
{
    std::vector<float> features;
};

class FingerprintGenerator
{
public:
    Fingerprint Generate(const std::vector<float>& samples);
};
}
