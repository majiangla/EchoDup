#pragma once

#include <cstddef>

namespace EchoDup::UI
{
class ScanProgressViewModel
{
public:
    void Update(std::size_t completed, std::size_t total);

    double Progress() const noexcept
    {
        return progress_;
    }

private:
    double progress_{0.0};
};
}
