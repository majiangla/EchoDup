#include "Similarity.h"
#include <cmath>

namespace EchoDup::Core
{

double SimilarityCalculator::Compare(
    const Fingerprint& a,
    const Fingerprint& b) const
{
    if(a.features.empty() || b.features.empty())
        return 0.0;

    double diff = std::abs(a.features[0]-b.features[0]);
    return 1.0 / (1.0 + diff);
}

}
