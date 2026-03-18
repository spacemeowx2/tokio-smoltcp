/// `BufferSize` is used to configure the size of the socket buffer.
#[derive(Debug, Clone, Copy)]
pub struct BufferSize {
    pub tcp_rx_size: usize,
    pub tcp_tx_size: usize,
    pub udp_rx_size: usize,
    pub udp_tx_size: usize,
    pub udp_rx_meta_size: usize,
    pub udp_tx_meta_size: usize,
    pub raw_rx_size: usize,
    pub raw_tx_size: usize,
    pub raw_rx_meta_size: usize,
    pub raw_tx_meta_size: usize,
}

impl Default for BufferSize {
    fn default() -> Self {
        Self {
            tcp_rx_size: 8192,
            tcp_tx_size: 8192,
            udp_rx_size: 8192,
            udp_tx_size: 8192,
            udp_rx_meta_size: 32,
            udp_tx_meta_size: 32,
            raw_rx_size: 8192,
            raw_tx_size: 8192,
            raw_rx_meta_size: 32,
            raw_tx_meta_size: 32,
        }
    }
}
