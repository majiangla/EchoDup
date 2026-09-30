#pragma once

#include "Fingerprint.h"

namespace EchoDup::Core
{
class SimilarityCalculator
{
public:
    double Compare(const Fingerprint& a, const Fingerprint& b) const;
};
}
