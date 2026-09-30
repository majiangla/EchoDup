#pragma once

#include <vector>
#include <cstdint>

namespace EchoDup::Core
{
struct Fingerprint
{
    std::vector<float> features;
};

class FingerprintGenerator
{
public:
    Fingerprint Generate(const std::vector<float>& samples) const;
};

}
