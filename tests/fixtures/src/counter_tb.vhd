library ieee; use ieee.std_logic_1164.all; use ieee.numeric_std.all;
entity counter_tb is end entity;
architecture sim of counter_tb is
  signal clk : std_logic := '0';
  signal rst : std_logic := '1';
  signal cnt : unsigned(7 downto 0) := (others => '0');
  signal sn  : signed(7 downto 0) := to_signed(-3, 8);
begin
  clk <= not clk after 5 ns;              -- posedges at 5,15,25,... ns
  rst <= '0' after 12 ns;
  process(clk) begin
    if rising_edge(clk) then
      if rst = '1' then cnt <= (others => '0'); else cnt <= cnt + 1; end if;
    end if;
  end process;
end architecture;
