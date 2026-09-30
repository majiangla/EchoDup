#include "Fingerprint.h"
#include <cmath>

namespace EchoDup::Core
{

Fingerprint FingerprintGenerator::Generate(
    const std::vector<float>& samples) const
{
    Fingerprint result;

    if(samples.empty())
        return result;

    float energy = 0.0f;
    for(float sample : samples)
        energy += sample * sample;

    result.features.push_back(
        std::sqrt(energy / static_cast<float>(samples.size())));

    return result;
}

}
