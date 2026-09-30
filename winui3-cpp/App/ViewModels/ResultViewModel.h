#pragma once

#include <vector>
#include <string>

struct ResultItem
{
    std::wstring first;
    std::wstring second;
    double similarity{};
};

class ResultViewModel
{
public:
    void AddResult(const ResultItem& item);
    const std::vector<ResultItem>& Results() const;

private:
    std::vector<ResultItem> results_;
};
