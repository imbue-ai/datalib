-- Phase variance across the aft array.
local function variance(readings)
  local sum, squares = 0, 0
  for _, r in ipairs(readings) do sum = sum + r end
  local mean = sum / #readings
  for _, r in ipairs(readings) do squares = squares + (r - mean) ^ 2 end
  return squares / #readings
end
